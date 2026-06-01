# Workspace Context Loading Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Load a workspace instruction file (`AGENTS.md`/`AGENT.md`/`CLAUDE.md`) and progressive-disclosure skills (`.claude/skills/<name>/SKILL.md`) from the runtime's working directory and surface them to every agent's system prompt, re-scanned on each agent spawn.

**Architecture:** A new parameterized `ScanWorkspace` runtime protocol op (fluorite) returns raw file contents over the existing `RuntimeClient` boundary — never host fs, so future runtime providers work unchanged. The `workflow` crate interprets the raw scan into a `WorkspaceContext` (parses skill frontmatter), composes it into each agent's system prompt at spawn, and exposes skill bodies on demand via a synthesized `skill` tool (which bypasses `allowed_tools`, like `conclude`).

**Tech Stack:** Rust (edition 2024), fluorite protocol codegen, tokio, tokio-tungstenite, the `glob` crate (already a `runtime` dep). No new dependencies — skill frontmatter is hand-parsed (the codebase has no YAML dep and adding one needs cargo-deny review).

**Reference spec:** `docs/superpowers/specs/2026-05-31-workspace-context-loading-design.md`

**Conventions for every task:**
- Production code denies `unwrap`, `expect`, `panic`, `wildcard_enum_match_arm`. Match all enum variants explicitly. Test modules opt out with the standard `#![cfg_attr(test, allow(...))]` / `#[allow(...)]` block already used across the codebase.
- After editing `fluorite/*.fl`, regenerate with `cargo build -p models`.
- **fluorite gotcha:** never put `///` doc comments on union *variants* — it silently breaks codegen (missing `OUT_DIR/<pkg>/mod.rs`). Comment the structs above the union instead.
- Commit messages: conventional, succinct, no AI attribution.

---

### Task 1: Add `ScanWorkspace` protocol types

**Files:**
- Modify: `fluorite/runtime.fl`
- Test: `models/src/lib.rs` (hand-written wire-format test)

- [ ] **Step 1: Add the new structs and union variants to the schema**

In `fluorite/runtime.fl`, after the `GrepInput` struct (line 23) and before `// --- Inbound`, add:

```
// --- Workspace scan ---

struct ScanRequest {
    call_id: String,
    instruction_candidates: Vec<String>,
    skills_glob: String,
}

struct ScannedFile { path: String, content: String }
struct WorkspaceScan { instructions: Option<ScannedFile>, skills: Vec<ScannedFile> }
struct ScanResponse { call_id: String, scan: WorkspaceScan }
```

Add the inbound variant — change the `RuntimeInboundMessage` union (lines 43-47) to:

```
#[type_tag = "type"]
union RuntimeInboundMessage {
    ToolCall(ToolCallRequest),
    CancelCall(CancelCallRequest),
    ScanWorkspace(ScanRequest),
}
```

Add the outbound variant — change the `RuntimeOutboundMessage` union (lines 63-67) to:

```
#[type_tag = "type"]
union RuntimeOutboundMessage {
    Ready(RuntimeReady),
    ToolCallResponse(ToolCallResponse),
    ScanResult(ScanResponse),
}
```

(No `///` comments inside the unions.)

- [ ] **Step 2: Regenerate and verify the types compile**

Run: `cargo build -p models`
Expected: builds clean. If it errors on `Vec<ScannedFile>` or `Option<ScannedFile>`, check an existing schema that nests structs (e.g. `fluorite/agent.fl`, `fluorite/events.fl`) for the exact supported syntax and match it.

- [ ] **Step 3: Write a wire-format test in `models/src/lib.rs`**

Add (inside or appended to the existing `#[cfg(test)] mod tests`, mirroring its `#[allow(...)]` header):

```rust
#[test]
fn scan_workspace_inbound_round_trips() {
    use crate::runtime::{RuntimeInboundMessage, ScanRequest};
    let msg = RuntimeInboundMessage::ScanWorkspace(ScanRequest {
        call_id: "c1".into(),
        instruction_candidates: vec!["AGENTS.md".into()],
        skills_glob: ".claude/skills/*/SKILL.md".into(),
    });
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"ScanWorkspace\""));
    let back: RuntimeInboundMessage = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, RuntimeInboundMessage::ScanWorkspace(r) if r.call_id == "c1"));
}

#[test]
fn scan_result_outbound_round_trips() {
    use crate::runtime::{RuntimeOutboundMessage, ScanResponse, ScannedFile, WorkspaceScan};
    let msg = RuntimeOutboundMessage::ScanResult(ScanResponse {
        call_id: "c1".into(),
        scan: WorkspaceScan {
            instructions: Some(ScannedFile { path: "AGENTS.md".into(), content: "hi".into() }),
            skills: vec![ScannedFile { path: ".claude/skills/x/SKILL.md".into(), content: "b".into() }],
        },
    });
    let json = serde_json::to_string(&msg).unwrap();
    assert!(json.contains("\"type\":\"ScanResult\""));
    let back: RuntimeOutboundMessage = serde_json::from_str(&json).unwrap();
    assert!(matches!(back, RuntimeOutboundMessage::ScanResult(r) if r.scan.skills.len() == 1));
}
```

Check `models/src/lib.rs` for how generated modules are re-exported (e.g. `pub mod runtime` or `models::runtime`); use whatever path the existing code uses (the rest of the codebase imports `models::runtime::...`).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p models scan_`
Expected: both tests PASS.

- [ ] **Step 5: Commit**

```bash
git add fluorite/runtime.fl models/src/lib.rs
git commit -m "feat(runtime): add ScanWorkspace protocol op types"
```

---

### Task 2: Runtime-side scan execution

**Files:**
- Create: `runtime/src/scan.rs`
- Modify: `runtime/src/lib.rs` (add `pub mod scan;`)
- Modify: `runtime/src/main.rs:140-198` (handle the new inbound variant)

- [ ] **Step 1: Write failing tests for `scan::exec`**

Create `runtime/src/scan.rs`:

```rust
use models::runtime::{ScanRequest, ScannedFile, WorkspaceScan};
use std::path::Path;

/// Gather workspace context from `working_dir`: the first existing instruction
/// candidate (in order) and every file matching `skills_glob`. Best-effort — a
/// missing candidate yields `None`; an unreadable match is skipped.
pub fn exec(working_dir: &Path, req: ScanRequest) -> WorkspaceScan {
    let instructions = req
        .instruction_candidates
        .iter()
        .find_map(|name| {
            let path = working_dir.join(name);
            std::fs::read_to_string(&path)
                .ok()
                .map(|content| ScannedFile { path: name.clone(), content })
        });

    let pattern = format!("{}/{}", working_dir.display(), req.skills_glob);
    let mut skills = Vec::new();
    if let Ok(paths) = glob::glob(&pattern) {
        for entry in paths.flatten() {
            if let Ok(content) = std::fs::read_to_string(&entry) {
                skills.push(ScannedFile {
                    path: entry.to_string_lossy().into_owned(),
                    content,
                });
            }
        }
    }

    WorkspaceScan { instructions, skills }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn req() -> ScanRequest {
        ScanRequest {
            call_id: "c".into(),
            instruction_candidates: vec!["AGENTS.md".into(), "AGENT.md".into(), "CLAUDE.md".into()],
            skills_glob: ".claude/skills/*/SKILL.md".into(),
        }
    }

    #[test]
    fn instruction_precedence_first_match_wins() {
        let dir = TempDir::new().unwrap();
        std::fs::write(dir.path().join("AGENT.md"), "second").unwrap();
        std::fs::write(dir.path().join("CLAUDE.md"), "third").unwrap();
        let scan = exec(dir.path(), req());
        let f = scan.instructions.expect("instructions");
        assert_eq!(f.path, "AGENT.md");
        assert_eq!(f.content, "second");
    }

    #[test]
    fn no_instruction_file_is_none() {
        let dir = TempDir::new().unwrap();
        assert!(exec(dir.path(), req()).instructions.is_none());
    }

    #[test]
    fn globs_skills_in_hidden_dir() {
        let dir = TempDir::new().unwrap();
        let skill_dir = dir.path().join(".claude/skills/git-bisect");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), "body").unwrap();
        let scan = exec(dir.path(), req());
        assert_eq!(scan.skills.len(), 1);
        assert_eq!(scan.skills[0].content, "body");
        assert!(scan.skills[0].path.ends_with(".claude/skills/git-bisect/SKILL.md"));
    }

    #[test]
    fn missing_skills_dir_is_empty() {
        let dir = TempDir::new().unwrap();
        assert!(exec(dir.path(), req()).skills.is_empty());
    }
}
```

Add `pub mod scan;` to `runtime/src/lib.rs` (read the file first; place it alongside the existing `pub mod tools;`).

- [ ] **Step 2: Run tests to verify they pass**

Run: `cargo test -p runtime scan::`
Expected: 4 tests PASS. (Implementation was written with the tests; if any fail, fix `scan.rs` — do not change the tests.)

- [ ] **Step 3: Wire the inbound variant in `runtime/src/main.rs`**

In `run_loop`, the inbound `match inbound { ... }` (lines 147-193) currently has two arms. Add a third arm after `CancelCall` (before the closing brace at line 193). Mirror the `ToolCall` arm's spawn+send shape:

```rust
                    RuntimeInboundMessage::ScanWorkspace(req) => {
                        let call_id = req.call_id.clone();
                        let working_dir = working_dir.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();

                        let handle = tokio::spawn(async move {
                            let scan = runtime::scan::exec(&working_dir, req);
                            let response = serde_json::to_string(
                                &RuntimeOutboundMessage::ScanResult(ScanResponse {
                                    call_id: call_id.clone(),
                                    scan,
                                }),
                            );
                            if let Ok(json) = response {
                                let _ = sink_clone.lock().await.send(Message::Text(json.into())).await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });

                        in_flight.lock().await.insert(req_call_id_for_map(&handle), handle.abort_handle());
                    }
```

Note: the `ToolCall` arm inserts under `req.call_id` but `req` was moved into the task. Match that arm's exact pattern — it clones `call_id` before the move and inserts under the original `req.call_id` (which is still available because only `req.call` is moved). For scan, `req` is moved whole into the task, so capture the id first:

```rust
                    RuntimeInboundMessage::ScanWorkspace(req) => {
                        let call_id = req.call_id.clone();
                        let map_id = req.call_id.clone();
                        let working_dir = working_dir.clone();
                        let sink_clone = sink.clone();
                        let in_flight_clone = in_flight.clone();
                        let handle = tokio::spawn(async move {
                            let scan = runtime::scan::exec(&working_dir, req);
                            if let Ok(json) = serde_json::to_string(
                                &RuntimeOutboundMessage::ScanResult(ScanResponse { call_id: call_id.clone(), scan }),
                            ) {
                                let _ = sink_clone.lock().await.send(Message::Text(json.into())).await;
                            }
                            in_flight_clone.lock().await.remove(&call_id);
                        });
                        in_flight.lock().await.insert(map_id, handle.abort_handle());
                    }
```

Update the `use models::runtime::{...}` block at the top of `main.rs` (lines 13-16) to also import `ScanResponse`.

- [ ] **Step 4: Verify the runtime crate builds**

Run: `cargo build -p runtime`
Expected: builds clean (the inbound `match` is now exhaustive again).

- [ ] **Step 5: Commit**

```bash
git add runtime/src/scan.rs runtime/src/lib.rs runtime/src/main.rs
git commit -m "feat(runtime): execute ScanWorkspace and reply with ScanResult"
```

---

### Task 3: Transport + client scan support (single buildable change)

Adding a `RuntimeTransport` trait method forces every implementor to implement it, so the trait method, `RuntimeClient` wrapper, `MockTransport`, and `SocketRuntimeTransport` all land together to keep the workspace building.

**Files:**
- Modify: `runtime-client/src/transport.rs` (trait method + `MockTransport`)
- Modify: `runtime-client/src/client.rs` (`RuntimeClient::scan_workspace`)
- Modify: `executor/src/socket_transport.rs` (`pending_scan` map + reader demux + impl)

- [ ] **Step 1: Add the trait method and a default-empty `MockTransport` impl**

In `runtime-client/src/transport.rs`:
- Extend the imports: `use models::runtime::{ToolCall, ToolOutput, ToolResult, WorkspaceScan};`
- Add to the `RuntimeTransport` trait (after `cancel`):

```rust
    async fn scan_workspace(
        &self,
        call_id: &str,
        instruction_candidates: Vec<String>,
        skills_glob: String,
    ) -> Result<WorkspaceScan, TransportError>;
```

- Give `MockTransport` an optional canned scan. Add a field and constructor; default to an empty scan:

```rust
pub struct MockTransport {
    result: ToolResult,
    scan: WorkspaceScan,
}
```

Update `ok` and `err` to set `scan: WorkspaceScan { instructions: None, skills: Vec::new() }`. Add:

```rust
    /// Override the canned scan returned by `scan_workspace`.
    pub fn with_scan(mut self, scan: WorkspaceScan) -> Self {
        self.scan = scan;
        self
    }
```

- Implement the new method on `MockTransport`:

```rust
    async fn scan_workspace(
        &self,
        _call_id: &str,
        _candidates: Vec<String>,
        _skills_glob: String,
    ) -> Result<WorkspaceScan, TransportError> {
        Ok(self.scan.clone())
    }
```

- [ ] **Step 2: Add `RuntimeClient::scan_workspace` with a test**

In `runtime-client/src/client.rs`:
- Extend imports: `use models::runtime::{ToolCall, ToolError, ToolOutput, ToolResult, WorkspaceScan};`
- Add the method to `impl RuntimeClient` (mirroring `invoke`'s `call_id` generation):

```rust
    pub async fn scan_workspace(
        &self,
        instruction_candidates: Vec<String>,
        skills_glob: String,
    ) -> Result<WorkspaceScan, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.inner
            .scan_workspace(&call_id, instruction_candidates, skills_glob)
            .await
            .map_err(RuntimeCallError::Transport)
    }
```

- Add a test in the existing `#[cfg(test)] mod tests`:

```rust
    #[tokio::test]
    async fn client_scan_returns_mock_scan() {
        use models::runtime::{ScannedFile, WorkspaceScan};
        let scan = WorkspaceScan {
            instructions: Some(ScannedFile { path: "AGENTS.md".into(), content: "hi".into() }),
            skills: vec![],
        };
        let client = RuntimeClient::new(MockTransport::ok("").with_scan(scan));
        let out = client
            .scan_workspace(vec!["AGENTS.md".into()], ".claude/skills/*/SKILL.md".into())
            .await
            .unwrap();
        assert_eq!(out.instructions.unwrap().content, "hi");
    }
```

- [ ] **Step 3: Add `pending_scan` + reader demux + impl in `SocketRuntimeTransport`**

In `executor/src/socket_transport.rs`:
- Extend imports: add `ScanRequest`, `ScanResponse`, `WorkspaceScan` to the `models::runtime::{...}` import.
- Add a type alias and field:

```rust
type ScanReply = Result<WorkspaceScan, TransportError>;
type PendingScan = Arc<Mutex<HashMap<String, oneshot::Sender<ScanReply>>>>;
```

Add `pending_scan: PendingScan` to the struct, initialize it in `from_split`, and clone it into the reader (`let reader_pending_scan = pending_scan.clone();`).

- Change the reader loop body (lines 50-59) from the `if let` to an exhaustive `match` so the new variant is handled and the lint passes:

```rust
            while let Some(Ok(Message::Text(text))) = stream.next().await {
                match serde_json::from_str::<RuntimeOutboundMessage>(&text) {
                    Ok(RuntimeOutboundMessage::ToolCallResponse(resp)) => {
                        if let Some(tx) = reader_pending.lock().await.remove(&resp.call_id) {
                            let _ = tx.send(Ok(resp.result));
                        }
                    }
                    Ok(RuntimeOutboundMessage::ScanResult(resp)) => {
                        if let Some(tx) = reader_pending_scan.lock().await.remove(&resp.call_id) {
                            let _ = tx.send(Ok(resp.scan));
                        }
                    }
                    Ok(RuntimeOutboundMessage::Ready(_)) | Err(_) => {}
                }
            }
```

- In the disconnect cleanup (lines 62-66), also drain `pending_scan`:

```rust
            let mut scan_map = reader_pending_scan.lock().await;
            for (_, tx) in scan_map.drain() {
                let _ = tx.send(Err(TransportError::Disconnected));
            }
            drop(scan_map);
```

- Add `pending_scan` to the returned `Self { ... }`.
- Implement the trait method (mirror `invoke`'s send-then-await, using the scan inbound message):

```rust
    async fn scan_workspace(
        &self,
        call_id: &str,
        instruction_candidates: Vec<String>,
        skills_glob: String,
    ) -> Result<WorkspaceScan, TransportError> {
        let (tx, rx) = oneshot::channel();
        self.pending_scan.lock().await.insert(call_id.to_string(), tx);
        let msg = RuntimeInboundMessage::ScanWorkspace(ScanRequest {
            call_id: call_id.to_string(),
            instruction_candidates,
            skills_glob,
        });
        let json = serde_json::to_string(&msg)
            .map_err(|e| TransportError::Serialization(e.to_string()))?;
        if let Err(e) = self.sink.lock().await.send(Message::Text(json.into())).await {
            self.pending_scan.lock().await.remove(call_id);
            return Err(TransportError::SendFailed(e.to_string()));
        }
        match rx.await {
            Ok(reply) => reply,
            Err(_) => Err(TransportError::Disconnected),
        }
    }
```

- [ ] **Step 4: Extend the `paired()` fake runtime + add a scan correlation test**

In the `socket_transport.rs` test module, the `paired()` fake answers `ToolCall`. Add a `ScanWorkspace` answer to its inner `while let` loop. Change its parse-and-match so it handles both inbound variants:

```rust
            while let Some(Ok(Message::Text(t))) = stream.next().await {
                match serde_json::from_str::<RuntimeInboundMessage>(&t) {
                    Ok(RuntimeInboundMessage::ToolCall(req)) => {
                        let resp = RuntimeOutboundMessage::ToolCallResponse(ToolCallResponse {
                            call_id: req.call_id,
                            result: ToolResult::Ok(ToolOutput {
                                stdout: "ok".into(), stderr: String::new(), exit_code: 0,
                            }),
                        });
                        let _ = sink.send(Message::Text(serde_json::to_string(&resp).unwrap().into())).await;
                    }
                    Ok(RuntimeInboundMessage::ScanWorkspace(req)) => {
                        let resp = RuntimeOutboundMessage::ScanResult(models::runtime::ScanResponse {
                            call_id: req.call_id,
                            scan: models::runtime::WorkspaceScan {
                                instructions: Some(models::runtime::ScannedFile {
                                    path: "AGENTS.md".into(), content: "ctx".into(),
                                }),
                                skills: vec![],
                            },
                        });
                        let _ = sink.send(Message::Text(serde_json::to_string(&resp).unwrap().into())).await;
                    }
                    Ok(RuntimeInboundMessage::CancelCall(_)) | Err(_) => {}
                }
            }
```

Add the test:

```rust
    #[tokio::test]
    async fn scan_correlates_response() {
        let (t, _dir) = paired().await;
        let scan = t
            .scan_workspace("s1", vec!["AGENTS.md".into()], ".claude/skills/*/SKILL.md".into())
            .await
            .unwrap();
        assert_eq!(scan.instructions.unwrap().content, "ctx");
    }
```

- [ ] **Step 5: Build and test the affected crates**

Run: `cargo test -p runtime-client -p executor`
Expected: all PASS, including `client_scan_returns_mock_scan` and `scan_correlates_response`.

- [ ] **Step 6: Verify the whole workspace still builds**

Run: `cargo build --workspace`
Expected: builds clean (no other `RuntimeTransport` implementors exist — confirmed by grep; if the build flags a missing impl somewhere, add the same `scan_workspace` body shape there).

- [ ] **Step 7: Commit**

```bash
git add runtime-client/src/transport.rs runtime-client/src/client.rs executor/src/socket_transport.rs
git commit -m "feat(runtime-client): scan_workspace transport + client method"
```

---

### Task 4: `workspace` module — context types, frontmatter parsing, prompt composition

**Files:**
- Create: `workflow/src/workspace.rs`
- Modify: `workflow/src/lib.rs` (add `mod workspace;` + re-exports)

- [ ] **Step 1: Write the module with tests (pure logic first)**

Create `workflow/src/workspace.rs`:

```rust
use models::runtime::{ScannedFile, WorkspaceScan};
use runtime_client::RuntimeClient;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Instruction filenames tried in order at the workdir root; first found wins.
const INSTRUCTION_CANDIDATES: &[&str] = &["AGENTS.md", "AGENT.md", "CLAUDE.md"];
/// Glob (relative to the workdir) locating skill definition files.
const SKILLS_GLOB: &str = ".claude/skills/*/SKILL.md";

/// Workspace context surfaced to every agent: the project instruction file and the
/// set of available skills, both as of the spawn-time scan.
#[derive(Clone, Default)]
pub struct WorkspaceContext {
    pub instructions: Option<String>,
    pub skills: Arc<SkillSet>,
}

/// Skills keyed by name, kept sorted for a stable prompt ordering.
#[derive(Default)]
pub struct SkillSet {
    skills: BTreeMap<String, Skill>,
}

#[derive(Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
}

impl SkillSet {
    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }
    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name)
    }
    pub fn names(&self) -> Vec<String> {
        self.skills.keys().cloned().collect()
    }
    fn iter(&self) -> impl Iterator<Item = &Skill> {
        self.skills.values()
    }
}

/// Scan the workspace over the runtime and interpret it. On a transport error,
/// warn and return an empty context — the feature is additive and must not sink a run.
pub async fn scan(client: &RuntimeClient) -> WorkspaceContext {
    let candidates = INSTRUCTION_CANDIDATES.iter().map(|s| s.to_string()).collect();
    match client.scan_workspace(candidates, SKILLS_GLOB.to_string()).await {
        Ok(raw) => interpret(raw),
        Err(e) => {
            tracing::warn!(error = %e, "workspace scan failed; continuing without it");
            WorkspaceContext::default()
        }
    }
}

fn interpret(raw: WorkspaceScan) -> WorkspaceContext {
    let instructions = raw.instructions.map(|f| f.content);
    let mut skills = BTreeMap::new();
    for file in raw.skills {
        match parse_skill(&file) {
            Some(skill) => {
                if skills.contains_key(&skill.name) {
                    tracing::warn!(name = %skill.name, "duplicate skill name; keeping first");
                } else {
                    skills.insert(skill.name.clone(), skill);
                }
            }
            None => tracing::warn!(path = %file.path, "skipping skill with invalid frontmatter"),
        }
    }
    WorkspaceContext {
        instructions,
        skills: Arc::new(SkillSet { skills }),
    }
}

/// Parse a `SKILL.md` with leading `---` YAML frontmatter into name/description/body.
/// Only flat `key: value` scalars are read (the SKILL.md convention); returns `None`
/// if the fence is missing or `name`/`description` are absent.
fn parse_skill(file: &ScannedFile) -> Option<Skill> {
    let (front, body) = split_frontmatter(&file.content)?;
    let mut name = None;
    let mut description = None;
    for line in front.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once(':')?;
        let value = unquote(value.trim());
        match key.trim() {
            "name" => name = Some(value.to_string()),
            "description" => description = Some(value.to_string()),
            _ => {}
        }
    }
    Some(Skill {
        name: name?,
        description: description?,
        body: body.trim_start().to_string(),
    })
}

/// Split `---\n<frontmatter>\n---\n<body>`; returns `(frontmatter, body)`.
fn split_frontmatter(content: &str) -> Option<(&str, &str)> {
    let rest = content.strip_prefix("---")?;
    let rest = rest.strip_prefix('\n').or_else(|| rest.strip_prefix("\r\n"))?;
    // Find a closing fence line (`---` possibly with trailing CR).
    let mut idx = 0;
    for line in rest.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        if trimmed == "---" {
            let front = &rest[..idx];
            let body = &rest[idx + line.len()..];
            return Some((front, body));
        }
        idx += line.len();
    }
    None
}

fn unquote(s: &str) -> &str {
    let bytes = s.as_bytes();
    if s.len() >= 2
        && ((bytes[0] == b'"' && bytes[s.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[s.len() - 1] == b'\''))
    {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

/// Compose the agent's effective system prompt: its own prompt first (role), then
/// the workspace instructions, then the available-skills listing. Sections are
/// omitted when empty; returns `None` if nothing at all would be emitted.
pub fn compose_system_prompt(agent_prompt: Option<&str>, ws: &WorkspaceContext) -> Option<String> {
    let mut sections: Vec<String> = Vec::new();
    if let Some(p) = agent_prompt {
        if !p.trim().is_empty() {
            sections.push(p.trim().to_string());
        }
    }
    if let Some(instr) = &ws.instructions {
        if !instr.trim().is_empty() {
            sections.push(format!("# Workspace context\n{}", instr.trim()));
        }
    }
    if !ws.skills.is_empty() {
        let mut block = String::from(
            "# Available skills\nLoad a skill's full instructions with the `skill` tool before relying on it.\n",
        );
        for s in ws.skills.iter() {
            block.push_str(&format!("- {}: {}\n", s.name, s.description));
        }
        sections.push(block.trim_end().to_string());
    }
    if sections.is_empty() {
        None
    } else {
        Some(sections.join("\n\n"))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]
mod tests {
    use super::*;

    fn file(path: &str, content: &str) -> ScannedFile {
        ScannedFile { path: path.into(), content: content.into() }
    }

    #[test]
    fn parses_valid_skill() {
        let s = parse_skill(&file(
            ".claude/skills/x/SKILL.md",
            "---\nname: git-bisect\ndescription: Find the bad commit\n---\nDo the bisect.\n",
        ))
        .unwrap();
        assert_eq!(s.name, "git-bisect");
        assert_eq!(s.description, "Find the bad commit");
        assert_eq!(s.body, "Do the bisect.");
    }

    #[test]
    fn description_with_colon_keeps_full_value() {
        let s = parse_skill(&file(
            "p",
            "---\nname: n\ndescription: Use when X: do Y\n---\nbody",
        ))
        .unwrap();
        assert_eq!(s.description, "Use when X: do Y");
    }

    #[test]
    fn strips_quotes() {
        let s = parse_skill(&file("p", "---\nname: \"n\"\ndescription: 'd'\n---\nb")).unwrap();
        assert_eq!(s.name, "n");
        assert_eq!(s.description, "d");
    }

    #[test]
    fn missing_fence_is_none() {
        assert!(parse_skill(&file("p", "name: n\ndescription: d\nbody")).is_none());
    }

    #[test]
    fn missing_required_key_is_none() {
        assert!(parse_skill(&file("p", "---\nname: n\n---\nbody")).is_none());
    }

    #[test]
    fn interpret_skips_bad_and_dedupes() {
        let raw = WorkspaceScan {
            instructions: Some(file("AGENTS.md", "proj")),
            skills: vec![
                file("a/SKILL.md", "---\nname: a\ndescription: first\n---\nbody-a"),
                file("b/SKILL.md", "no frontmatter"),
                file("c/SKILL.md", "---\nname: a\ndescription: dup\n---\nbody-dup"),
            ],
        };
        let ctx = interpret(raw);
        assert_eq!(ctx.instructions.as_deref(), Some("proj"));
        assert_eq!(ctx.skills.names(), vec!["a".to_string()]);
        assert_eq!(ctx.skills.get("a").unwrap().description, "first");
    }

    #[test]
    fn compose_is_role_first_and_omits_empty() {
        let ctx = WorkspaceContext {
            instructions: Some("project rules".into()),
            skills: Arc::new(SkillSet {
                skills: [(
                    "git-bisect".to_string(),
                    Skill { name: "git-bisect".into(), description: "find bad commit".into(), body: "b".into() },
                )]
                .into_iter()
                .collect(),
            }),
        };
        let prompt = compose_system_prompt(Some("You are a coder."), &ctx).unwrap();
        let role = prompt.find("You are a coder.").unwrap();
        let ctx_pos = prompt.find("# Workspace context").unwrap();
        let skills_pos = prompt.find("# Available skills").unwrap();
        assert!(role < ctx_pos && ctx_pos < skills_pos);
        assert!(prompt.contains("- git-bisect: find bad commit"));
    }

    #[test]
    fn compose_empty_context_is_none() {
        let ctx = WorkspaceContext::default();
        assert!(compose_system_prompt(None, &ctx).is_none());
        assert_eq!(compose_system_prompt(Some("just role"), &ctx).as_deref(), Some("just role"));
    }
}
```

Add to `workflow/src/lib.rs` (read it first to match its re-export style): `mod workspace;` and `pub use workspace::{compose_system_prompt, scan as scan_workspace, Skill, SkillSet, WorkspaceContext};`.

- [ ] **Step 2: Run the tests**

Run: `cargo test -p workflow workspace::`
Expected: all 8 tests PASS.

- [ ] **Step 3: Commit**

```bash
git add workflow/src/workspace.rs workflow/src/lib.rs
git commit -m "feat(workflow): workspace scan interpretation + prompt composition"
```

---

### Task 5: Synthesize the `skill` tool in the agent toolbox

**Files:**
- Modify: `workflow/src/context.rs` (`AgentToolbox`, `ToolboxFactory::for_agent`, `DefaultToolboxFactory`)
- Modify: `workflow/src/workflow_actor.rs:166-169` (pass an empty `SkillSet` for now — real one arrives in Task 6)

- [ ] **Step 1: Add a failing test for the skill tool**

In `workflow/src/context.rs` test module, add (it will not compile until the impl is in place — that's the failing state):

```rust
    #[tokio::test]
    async fn skill_tool_advertised_and_serves_body() {
        use crate::workspace::{Skill, SkillSet};
        let client = RuntimeClient::new(MockTransport::ok(""));
        let skills = Arc::new(SkillSet::from_iter([Skill {
            name: "git-bisect".into(),
            description: "find bad commit".into(),
            body: "Step 1...".into(),
        }]));
        let tb = DefaultToolboxFactory.for_agent(&def(None, None, false), client, skills);
        assert!(tb.specs().iter().any(|s| s.name == "skill"));
        let out = tb.execute("skill", json!({ "name": "git-bisect" })).await.unwrap();
        assert_eq!(out, json!("Step 1..."));
        let err = tb.execute("skill", json!({ "name": "nope" })).await.unwrap_err();
        assert!(matches!(err, ToolCallError::InvalidInput(_)));
    }

    #[tokio::test]
    async fn no_skill_tool_when_empty() {
        use crate::workspace::SkillSet;
        let client = RuntimeClient::new(MockTransport::ok(""));
        let tb = DefaultToolboxFactory.for_agent(&def(None, None, false), client, Arc::new(SkillSet::default()));
        assert!(!tb.specs().iter().any(|s| s.name == "skill"));
    }
```

Add a `SkillSet::from_iter` helper to `workflow/src/workspace.rs` so tests can build a set:

```rust
impl FromIterator<Skill> for SkillSet {
    fn from_iter<I: IntoIterator<Item = Skill>>(iter: I) -> Self {
        Self { skills: iter.into_iter().map(|s| (s.name.clone(), s)).collect() }
    }
}
```

- [ ] **Step 2: Implement the skill tool layer**

In `workflow/src/context.rs`:
- Add imports: `use crate::workspace::SkillSet;` and ensure `serde_json::json` is in scope where needed (the `ask_schema`/`both_schema` already use `json!`).
- Add a constant near `CONCLUDE_TOOL`: `pub const SKILL_TOOL: &str = "skill";`
- Change the `ToolboxFactory` trait method signature to thread the skills through:

```rust
pub trait ToolboxFactory: Send + Sync + 'static {
    fn for_agent(
        &self,
        agent_def: &WorkflowAgentDef,
        runtime_client: RuntimeClient,
        skills: Arc<SkillSet>,
    ) -> Arc<dyn Toolbox>;
}
```

- Update `DefaultToolboxFactory::for_agent` to pass `skills` into `AgentToolbox`:

```rust
        let conclude =
            conclude_tool_spec(agent_def.output_schema.as_ref(), agent_def.allow_ask_user);
        Arc::new(AgentToolbox { base, conclude, skills })
```

- Extend `AgentToolbox` and its `Toolbox` impl:

```rust
struct AgentToolbox {
    base: Arc<dyn Toolbox>,
    conclude: Option<ToolSpec>,
    skills: Arc<SkillSet>,
}

#[async_trait]
impl Toolbox for AgentToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        let mut specs = self.base.specs();
        if let Some(c) = &self.conclude {
            specs.push(c.clone());
        }
        if !self.skills.is_empty() {
            specs.push(ToolSpec {
                name: SKILL_TOOL.to_string(),
                description:
                    "Load the full instructions for a named skill listed under 'Available skills'."
                        .to_string(),
                input_schema: json!({
                    "type": "object",
                    "required": ["name"],
                    "properties": { "name": { "type": "string", "description": "The skill name." } }
                }),
            });
        }
        specs
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        if let Some(c) = &self.conclude
            && name == c.name
        {
            return Err(ToolCallError::ExecutionFailed(
                "the conclude tool is terminal and is not executed".to_string(),
            ));
        }
        if name == SKILL_TOOL {
            let requested = input.get("name").and_then(Value::as_str).unwrap_or_default();
            return match self.skills.get(requested) {
                Some(skill) => Ok(Value::String(skill.body.clone())),
                None => Err(ToolCallError::InvalidInput(format!(
                    "unknown skill '{requested}'; available: {}",
                    self.skills.names().join(", ")
                ))),
            };
        }
        self.base.execute(name, input).await
    }
}
```

- [ ] **Step 3: Fix the existing `for_agent` call site to keep the build green**

In `workflow/src/workflow_actor.rs`, `spawn_agent` (lines 166-169) currently calls `for_agent(agent_def, self.rt.runtime_client.clone())`. Update to pass an empty set for now (Task 6 replaces it with the scanned set):

```rust
        let toolbox = self.rt.toolbox_factory.for_agent(
            agent_def,
            self.rt.runtime_client.clone(),
            std::sync::Arc::new(crate::workspace::SkillSet::default()),
        );
```

Also fix the existing `for_agent` tests in `context.rs` (`toolbox_includes_conclude_and_filters_runtime_tools`, `conclude_tool_is_not_executable`) to pass the new third arg: `Arc::new(SkillSet::default())`. Ensure `use std::sync::Arc;` is present in the test module.

- [ ] **Step 4: Run tests**

Run: `cargo test -p workflow context::`
Expected: the two new skill-tool tests PASS plus the updated existing toolbox tests.

- [ ] **Step 5: Verify the workspace builds**

Run: `cargo build --workspace`
Expected: clean.

- [ ] **Step 6: Commit**

```bash
git add workflow/src/context.rs workflow/src/workflow_actor.rs workflow/src/workspace.rs
git commit -m "feat(workflow): synthesize the skill tool in the agent toolbox"
```

---

### Task 6: Scan per spawn and compose into every agent's prompt

**Files:**
- Modify: `workflow/src/workflow_actor.rs` (`spawn_agent` → async; scan + compose; update 4 call sites)

- [ ] **Step 1: Make `spawn_agent` async and scan + compose inside it**

Replace `spawn_agent` (lines 156-179) with:

```rust
    async fn spawn_agent(
        &self,
        ctx: &ActorContext<Self>,
        agent_def: &WorkflowAgentDef,
        session_id: Uuid,
    ) -> Result<ActorRef<AgentCommand>, String> {
        let provider = self
            .rt
            .provider_for(&agent_def.model)
            .ok_or_else(|| format!("no provider registered for model '{}'", agent_def.model))?;
        // Re-scan the workspace on every spawn so a mid-run `git pull`/edit by an
        // earlier agent is visible to this one (the prompt is fixed for the turn).
        let ws = crate::workspace::scan(&self.rt.runtime_client).await;
        let toolbox = self.rt.toolbox_factory.for_agent(
            agent_def,
            self.rt.runtime_client.clone(),
            ws.skills.clone(),
        );
        let agent_ctx = AgentRuntimeContext {
            provider,
            toolbox,
            event_sink: self.rt.event_sink.clone(),
            parent_ref: ctx.self_ref(),
            session_id,
        };
        let mut params = AgentParams::from_def(agent_def);
        params.system_prompt =
            crate::workspace::compose_system_prompt(agent_def.system_prompt.as_deref(), &ws);
        Ok(ctx.spawn(AgentActor::new(agent_ctx, params)))
    }
```

- [ ] **Step 2: Await `spawn_agent` at all call sites**

`spawn_agent` is called at five places — update each `self.spawn_agent(...)` to `self.spawn_agent(...).await`:
- `on_start` (≈ line 223): `match self.spawn_agent(ctx, &agent_def, session_id).await {`
- `on_concluded` (≈ line 299): `match self.spawn_agent(ctx, &to_def, to_session).await {`
- `on_resume` await branch (≈ line 363): `match self.spawn_agent(ctx, &agent_def, session_id).await {`
- `on_resume` suspended branch (≈ line 408): `match self.spawn_agent(ctx, &agent_def, session_id).await {`
- `on_fork` (≈ line 467): `match self.spawn_agent(ctx, &agent_def, new_session).await {`

(All five callers are already `async fn`, so no signature changes ripple further.)

- [ ] **Step 3: Build and run the full workflow test suite**

Run: `cargo test -p workflow`
Expected: all existing workflow tests still PASS (the `apply_event`/transition unit tests are unaffected; `spawn_agent` is exercised via Task 7's integration test).

- [ ] **Step 4: Commit**

```bash
git add workflow/src/workflow_actor.rs
git commit -m "feat(workflow): re-scan workspace and compose prompt per agent spawn"
```

---

### Task 7: Integration test — scan → context → toolbox → skill tool

**Files:**
- Create: `workflow/tests/workspace_context.rs`

- [ ] **Step 1: Write the integration test**

This exercises the real seam used by `spawn_agent` — `workspace::scan` over a `RuntimeClient` (backed by `MockTransport` returning a `WorkspaceScan`), then `compose_system_prompt` and the `DefaultToolboxFactory` skill tool — without standing up the full actor/journal.

Create `workflow/tests/workspace_context.rs`:

```rust
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::wildcard_enum_match_arm)]

use models::runtime::{ScannedFile, WorkspaceScan};
use models::workflow::WorkflowAgentDef;
use runtime_client::{MockTransport, RuntimeClient};
use std::sync::Arc;
use workflow::{compose_system_prompt, scan_workspace, DefaultToolboxFactory, ToolboxFactory};

fn agent_def() -> WorkflowAgentDef {
    WorkflowAgentDef {
        name: "coder".into(),
        system_prompt: Some("You are a coder.".into()),
        model: "m".into(),
        output_schema: None,
        allow_ask_user: false,
        transitions: None,
        max_iterations: None,
        max_retries: None,
        allowed_tools: Some(vec!["bash".into()]),
    }
}

fn scan_payload() -> WorkspaceScan {
    WorkspaceScan {
        instructions: Some(ScannedFile { path: "AGENTS.md".into(), content: "Project rules.".into() }),
        skills: vec![ScannedFile {
            path: ".claude/skills/git-bisect/SKILL.md".into(),
            content: "---\nname: git-bisect\ndescription: Find the bad commit\n---\nRun git bisect.".into(),
        }],
    }
}

#[tokio::test]
async fn scan_composes_prompt_and_exposes_skill_tool() {
    let client = RuntimeClient::new(MockTransport::ok("").with_scan(scan_payload()));
    let ws = scan_workspace(&client).await;

    // Prompt: role first, then workspace context, then the skill listing.
    let prompt = compose_system_prompt(agent_def().system_prompt.as_deref(), &ws).unwrap();
    assert!(prompt.contains("You are a coder."));
    assert!(prompt.contains("# Workspace context\nProject rules."));
    assert!(prompt.contains("- git-bisect: Find the bad commit"));

    // Toolbox: skill tool present (even though allowed_tools is just ["bash"]) and serves the body.
    let tb = DefaultToolboxFactory.for_agent(&agent_def(), client, ws.skills.clone());
    let names: Vec<String> = tb.specs().into_iter().map(|s| s.name).collect();
    assert!(names.contains(&"bash".to_string()));
    assert!(names.contains(&"skill".to_string()));
    let body = tb.execute("skill", serde_json::json!({ "name": "git-bisect" })).await.unwrap();
    assert_eq!(body, serde_json::json!("Run git bisect."));
}

#[tokio::test]
async fn empty_workspace_yields_plain_prompt_and_no_skill_tool() {
    let client = RuntimeClient::new(MockTransport::ok("")); // default empty scan
    let ws = scan_workspace(&client).await;
    let prompt = compose_system_prompt(agent_def().system_prompt.as_deref(), &ws);
    assert_eq!(prompt.as_deref(), Some("You are a coder."));
    let tb = DefaultToolboxFactory.for_agent(&agent_def(), client, ws.skills.clone());
    assert!(!tb.specs().iter().any(|s| s.name == "skill"));
}
```

Ensure the symbols used are exported from `workflow/src/lib.rs`: `compose_system_prompt`, `scan_workspace`, `DefaultToolboxFactory`, `ToolboxFactory`. Add any missing `pub use` (e.g. `pub use context::{DefaultToolboxFactory, ToolboxFactory};` if not already public). Confirm `MockTransport` is re-exported from `runtime_client` (it is used in `context.rs` as `runtime_client::MockTransport`).

- [ ] **Step 2: Run the integration test**

Run: `cargo test -p workflow --test workspace_context`
Expected: both tests PASS.

- [ ] **Step 3: Commit**

```bash
git add workflow/tests/workspace_context.rs workflow/src/lib.rs
git commit -m "test(workflow): workspace context scan + skill tool integration"
```

---

### Task 8: Full verification + push

**Files:** none (verification only)

- [ ] **Step 1: Format**

Run: `cargo fmt`
Then: `cargo fmt --check`
Expected: no diff.

- [ ] **Step 2: Clippy across the workspace**

Run: `cargo clippy --all-targets --all-features -- -D warnings`
Expected: no warnings. Common things to fix if they surface: a non-exhaustive `match` on `RuntimeInboundMessage`/`RuntimeOutboundMessage` (add the missing arm), or an `unwrap` that slipped into non-test code (rewrite with `?`/`match`).

- [ ] **Step 3: Full test suite**

Run: `cargo test --workspace`
Expected: all green, including the new `models`, `runtime`, `runtime-client`, `executor`, and `workflow` tests.

- [ ] **Step 4: Push to the PR branch**

```bash
git push
```

Expected: pushes the implementation commits onto `skills-support` (PR #35).

---

## Self-Review

**Spec coverage:**
- ScanWorkspace op (parameterized, raw return) → Task 1 (types) + Task 2 (runtime exec).
- Scan over the runtime boundary / transport correlation → Task 3.
- WorkspaceContext, frontmatter parsing, best-effort skip/dedupe, prompt composition → Task 4.
- Progressive-disclosure `skill` tool, bypasses `allowed_tools` → Task 5.
- Re-scan per agent spawn, compose into every agent's prompt → Task 6.
- All-agents-always, resume re-scans (drive re-runs spawn) → Task 6 (scan lives in spawn_agent, hit by every spawn including resume/fork).
- Integration coverage → Task 7. Verification gates → Task 8.

**Placeholder scan:** No TBD/TODO; every code step shows complete code. The one approximate detail (line numbers in `main.rs`/`workflow_actor.rs`) is anchored to named functions and surrounding code, and the `≈` markers tell the worker to locate by context.

**Type consistency:** `WorkspaceScan`/`ScannedFile`/`ScanRequest`/`ScanResponse` (protocol, `models::runtime`) vs. `WorkspaceContext`/`SkillSet`/`Skill` (domain, `workflow::workspace`) are used consistently. `for_agent` gains `Arc<SkillSet>` in Task 5 and is called with it in Tasks 5 (empty) and 6 (scanned). `scan_workspace` is the re-export of `workspace::scan`. `compose_system_prompt(Option<&str>, &WorkspaceContext) -> Option<String>` signature matches all call sites.

**Build-green ordering:** Each task ends in a buildable, testable state — Task 3 bundles the trait method with all implementors; Task 5 bridges the `for_agent` signature change with an empty `SkillSet` so the call site compiles before Task 6 supplies the scanned one.
