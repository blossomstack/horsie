# Plugin Hooks Dispatch Layer Implementation Plan (PR1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give horsie a real hook dispatch layer — classify all 31 Claude Code hook events, run hooks where the plugin files live, and let them block and amend tool calls — with `PreToolUse` and `PostToolUse` as the first consumers. PR2 adds the remaining five events and the failure gates.

**Architecture:** `horsie-support::plugin::hooks` owns event classification, tool-name aliasing and matcher evaluation. The runtime gains two protocol messages: a manifest (what hooks exist) and a general `RunHook` (execute one event, return a structured outcome). The server fetches the manifest once per session and consults it before every tool call, round-tripping only on a match; a `HookedToolbox` decorator slotted into the existing toolbox stack turns outcomes into denials, rewritten inputs and rewritten outputs. The agent loop is untouched.

**Tech Stack:** Rust 1.96.0, fluorite codegen (`models/fluorite/runtime.fl`), serde_json, regex, tokio, tempfile.

Spec: `docs/superpowers/specs/2026-08-02-plugin-hooks-design.md` (PR1 of two).

## Global Constraints

- Protocol types are ONLY defined in `models/fluorite/*.fl` (codegen). Never hand-write protocol structs.
- Production code denies `unwrap_used`, `expect_used`, `panic`, `wildcard_enum_match_arm` (workspace lints).
- Test modules open with the standard opt-out:
  ```rust
  #[cfg(test)]
  #[allow(
      clippy::unwrap_used,
      clippy::expect_used,
      clippy::panic,
      clippy::wildcard_enum_match_arm
  )]
  mod tests {
  ```
- Unit tests live in-file under `#[cfg(test)] mod tests`, using `tempfile::TempDir`.
- Tests must not touch the network.
- Avoid mutating process env (`std::env::set_var`) in tests — it is process-global and races with parallel tests.
- **PR1 supports exactly three events:** `SessionStart` (already wired via the legacy path), `PreToolUse` and `PostToolUse`. The spec's other five — `SessionEnd`, `UserPromptSubmit`, `Stop`, `SubagentStart`, `SubagentStop` — classify as `NotImplemented` here and are promoted in PR2, when their call sites exist. A classifier must never claim an event it does not run.
- **The install and session-start failure gates land in PR2, with those five.** Gating in PR1 would reject impeccable — which declares `Stop` — and un-ship what Phase 0 delivered. PR1 logs unsupported events as warnings, which is the status quo; PR2 turns them into hard failures once nothing legitimate is caught by them.
- **Fail closed on `PreToolUse` only:** a hook that times out, fails to spawn, or exits non-zero-and-not-2 denies the tool. No other event blocks on failure.
- **`permissionDecision: "ask"` and `"defer"` are treated as allow**, and logged.
- Pre-PR gates, in order: `cargo fmt --all`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test --workspace`.

## File Structure

**Created:**
- `support/src/plugin/hooks.rs` — `HookEvent`, `Unsupported`, `HookDecl`, `PluginHooks`, `read`, matcher evaluation, tool-name aliases.
- `server/src/sessions/hooked_toolbox.rs` — the `Toolbox` decorator.
- `runtime/src/hooks.rs` — manifest construction and per-event execution with the control protocol.

**Modified:**
- `support/Cargo.toml` — add `regex`.
- `support/src/plugin/mod.rs` — declare `hooks`.
- `models/fluorite/runtime.fl` — manifest + `RunHook` messages.
- `runtime-client/src/transport.rs` — two default methods.
- `runtime-client/src/client.rs` — two wrappers.
- `runtime-client/src/testkit.rs` — answer the new messages.
- `runtime/src/main.rs` — dispatch the new inbound messages.
- `runtime/src/plugins.rs` — `session_start_commands` moves onto the shared reader.
- `server/src/sessions/mod.rs` — declare `hooked_toolbox`.
- `server/src/sessions/session_actor.rs` — fetch the manifest and wrap the toolbox.
- `docs/guide/skills-and-plugins.md` — document hook support.

---

### Task 1: Event classification

**Files:**
- Create: `support/src/plugin/hooks.rs`
- Modify: `support/src/plugin/mod.rs`, `support/Cargo.toml`

**Interfaces:**
- Produces:
  ```rust
  pub enum HookEvent { SessionStart, PreToolUse, PostToolUse }
  pub enum Unsupported { NotImplemented, NoConcept, Unknown }
  impl HookEvent {
      pub fn parse(name: &str) -> Result<HookEvent, Unsupported>;
      pub fn as_str(self) -> &'static str;
  }
  impl Unsupported { pub fn explain(&self, event: &str) -> String; }
  ```

- [ ] **Step 1: Add the dependency**

In `support/Cargo.toml`, under `[dependencies]`, after `serde_json`:

```toml
regex      = "1"
```

- [ ] **Step 2: Write the failing tests**

Create `support/src/plugin/hooks.rs`:

```rust
//! Reading and classifying `<plugin>/hooks/hooks.json`.
//!
//! Claude Code documents 31 hook events. horsie classifies every one of them and
//! runs only those it has a call site for — it must never claim to run a guard
//! it silently drops. The classification is exhaustive so an event added
//! upstream surfaces as `Unknown` rather than passing for something supported.

use std::path::Path;

/// A hook event horsie can actually run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookEvent {
    SessionStart,
    PreToolUse,
    PostToolUse,
    // PR2 adds SessionEnd, UserPromptSubmit, Stop, SubagentStart, SubagentStop
    // alongside their call sites.
}

/// Why an event cannot run, which decides what the error tells the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Unsupported {
    /// horsie has the seam; no published plugin uses it, so it is not built.
    NotImplemented,
    /// horsie has no such concept.
    NoConcept,
    /// Not a documented Claude Code event at all.
    Unknown,
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;

    /// Every documented Claude Code event, as of the 2026-08-02 docs.
    const ALL_31: [&str; 31] = [
        "SessionStart", "Setup", "UserPromptSubmit", "UserPromptExpansion",
        "PreToolUse", "PermissionRequest", "PermissionDenied", "PostToolUse",
        "PostToolUseFailure", "PostToolBatch", "Notification", "MessageDisplay",
        "SubagentStart", "SubagentStop", "TaskCreated", "TaskCompleted", "Stop",
        "StopFailure", "TeammateIdle", "InstructionsLoaded", "ConfigChange",
        "CwdChanged", "DirectoryAdded", "FileChanged", "WorktreeCreate",
        "WorktreeRemove", "PreCompact", "PostCompact", "Elicitation",
        "ElicitationResult", "SessionEnd",
    ];

    /// Pins the contract: adding a variant cannot silently change what horsie
    /// claims to support.
    #[test]
    fn every_documented_event_is_classified_with_the_expected_counts() {
        let mut supported = 0;
        let mut not_implemented = 0;
        let mut no_concept = 0;
        for name in ALL_31 {
            match HookEvent::parse(name) {
                Ok(_) => supported += 1,
                Err(Unsupported::NotImplemented) => not_implemented += 1,
                Err(Unsupported::NoConcept) => no_concept += 1,
                Err(Unsupported::Unknown) => {
                    panic!("{name} is documented but classified Unknown")
                }
            }
        }
        // PR2 promotes five events out of `not_implemented`, making these
        // 8 / 7 / 16. Changing them is a deliberate act, not a side effect.
        assert_eq!(supported, 3, "supported set changed");
        assert_eq!(not_implemented, 12, "deferred set changed");
        assert_eq!(no_concept, 16, "absent set changed");
    }

    #[test]
    fn the_supported_three_are_exactly_these() {
        for name in ["SessionStart", "PreToolUse", "PostToolUse"] {
            assert!(HookEvent::parse(name).is_ok(), "{name} must be supported");
        }
    }

    /// The five PR2 events must classify as deferred, not supported — horsie
    /// has no call site for them yet, and claiming them would mean a hook that
    /// installs and silently never fires.
    #[test]
    fn the_pr2_events_are_deferred_until_their_call_sites_exist() {
        for name in [
            "SessionEnd",
            "UserPromptSubmit",
            "Stop",
            "SubagentStart",
            "SubagentStop",
        ] {
            assert_eq!(HookEvent::parse(name), Err(Unsupported::NotImplemented));
        }
    }

    #[test]
    fn round_trips_through_as_str() {
        for name in ["PreToolUse", "PostToolUse", "SessionStart"] {
            assert_eq!(HookEvent::parse(name).unwrap().as_str(), name);
        }
    }

    #[test]
    fn an_undocumented_name_is_unknown_not_a_silent_pass() {
        assert_eq!(HookEvent::parse("PreFlarbulate"), Err(Unsupported::Unknown));
        assert_eq!(HookEvent::parse("pretooluse"), Err(Unsupported::Unknown));
    }

    #[test]
    fn each_reason_explains_itself_differently() {
        let deferred = Unsupported::NotImplemented.explain("PostToolBatch");
        assert!(deferred.contains("PostToolBatch"), "{deferred}");
        assert!(deferred.contains("not implemented"), "{deferred}");

        let absent = Unsupported::NoConcept.explain("WorktreeCreate");
        assert!(absent.contains("no "), "{absent}");

        let unknown = Unsupported::Unknown.explain("Nonsense");
        assert!(unknown.contains("not a known"), "{unknown}");
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p horsie-support hooks`
Expected: FAIL — `HookEvent::parse` not defined.

- [ ] **Step 4: Implement classification**

Insert above the `#[cfg(test)]` block:

```rust
impl HookEvent {
    /// Classify a documented event name. `Err` carries why horsie cannot run it.
    pub fn parse(name: &str) -> Result<HookEvent, Unsupported> {
        match name {
            "SessionStart" => Ok(HookEvent::SessionStart),
            "PreToolUse" => Ok(HookEvent::PreToolUse),
            "PostToolUse" => Ok(HookEvent::PostToolUse),

            // Wired in PR2, when their call sites land. Classified as deferred
            // rather than supported so no hook can install and silently no-op.
            "SessionEnd" | "UserPromptSubmit" | "Stop" | "SubagentStart"
            | "SubagentStop"
            // horsie has the seam; nothing published uses these, so they are
            // deliberately not built. Promoting one is a small change.
            | "PostToolUseFailure" | "PostToolBatch" | "StopFailure" | "Notification"
            | "TaskCreated" | "TaskCompleted" | "CwdChanged" => {
                Err(Unsupported::NotImplemented)
            }

            // No horsie concept: no slash commands, no permission model, no
            // context compaction, no file watcher, no worktrees, no agent
            // teams, no MCP elicitation, no display layer.
            "UserPromptExpansion" | "PermissionRequest" | "PermissionDenied"
            | "PreCompact" | "PostCompact" | "FileChanged" | "ConfigChange"
            | "DirectoryAdded" | "Setup" | "MessageDisplay" | "TeammateIdle"
            | "WorktreeCreate" | "WorktreeRemove" | "Elicitation"
            | "ElicitationResult" | "InstructionsLoaded" => Err(Unsupported::NoConcept),

            _ => Err(Unsupported::Unknown),
        }
    }

    /// The documented name, so an outcome can be attributed on the wire.
    pub fn as_str(self) -> &'static str {
        match self {
            HookEvent::SessionStart => "SessionStart",
            HookEvent::PreToolUse => "PreToolUse",
            HookEvent::PostToolUse => "PostToolUse",
        }
    }
}

impl Unsupported {
    /// A sentence naming the event and what the user can do about it.
    pub fn explain(&self, event: &str) -> String {
        match self {
            Unsupported::NotImplemented => format!(
                "'{event}' is a Claude Code hook horsie has not implemented yet \
                 (no published plugin uses it) — open an issue if you need it"
            ),
            Unsupported::NoConcept => format!(
                "'{event}' has no equivalent in horsie, so its hook can never run"
            ),
            Unsupported::Unknown => {
                format!("'{event}' is not a known Claude Code hook event")
            }
        }
    }
}
```

Add `pub mod hooks;` to `support/src/plugin/mod.rs`, alphabetically after `grants`.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p horsie-support hooks`
Expected: PASS (6 tests)

- [ ] **Step 6: Commit**

```bash
git add support/ Cargo.lock
git commit -m "feat(support): classify all 31 Claude Code hook events"
```

---

### Task 2: Tool-name aliasing and matcher evaluation

**Files:**
- Modify: `support/src/plugin/hooks.rs`

**Interfaces:**
- Consumes: nothing from Task 1 beyond the module.
- Produces:
  ```rust
  pub fn claude_aliases(horsie_tool: &str) -> &'static [&'static str];
  pub fn matcher_applies(matcher: Option<&str>, horsie_tool: &str) -> bool;
  ```

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `support/src/plugin/hooks.rs`:

```rust
    /// The three matchers that actually exist across the official marketplace.
    /// If aliasing regresses, no published plugin's hook fires at all.
    #[test]
    fn real_world_matchers_hit_the_right_horsie_tools() {
        let edits = Some("Edit|Write|MultiEdit|NotebookEdit");
        assert!(matcher_applies(edits, "write_file"));
        assert!(matcher_applies(edits, "find_and_replace"));
        assert!(matcher_applies(edits, "replace_lines"));
        assert!(!matcher_applies(edits, "bash"));
        assert!(!matcher_applies(edits, "read_file"));

        assert!(matcher_applies(Some("Bash"), "bash"));
        assert!(!matcher_applies(Some("Bash"), "write_file"));
    }

    #[test]
    fn an_absent_or_empty_matcher_matches_everything() {
        assert!(matcher_applies(None, "bash"));
        assert!(matcher_applies(Some(""), "read_file"));
    }

    #[test]
    fn a_matcher_may_name_the_horsie_tool_directly() {
        assert!(matcher_applies(Some("write_file"), "write_file"));
    }

    /// Anchors are used in the wild (`^claude-security:claude-security$`), so
    /// matchers are real regexes rather than split alternations.
    #[test]
    fn anchors_are_honoured() {
        assert!(matcher_applies(Some("^bash$"), "bash"));
        assert!(!matcher_applies(Some("^ash$"), "bash"));
    }

    /// A matcher that will not compile must not match everything by accident.
    #[test]
    fn an_invalid_regex_matches_nothing() {
        assert!(!matcher_applies(Some("(unclosed"), "bash"));
    }

    #[test]
    fn tools_without_a_claude_equivalent_alias_to_nothing() {
        assert!(claude_aliases("set_env").is_empty());
        assert_eq!(claude_aliases("bash"), ["Bash"]);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-support hooks`
Expected: FAIL — `matcher_applies` not defined.

- [ ] **Step 3: Implement**

Insert above the `#[cfg(test)]` block in `support/src/plugin/hooks.rs`:

```rust
/// Claude Code tool names for a horsie tool.
///
/// Matchers published for Claude Code name Claude's tools — every matcher in
/// the official marketplace is one of `Bash` or
/// `Edit|Write|MultiEdit|NotebookEdit`. horsie's tools are snake_case and match
/// none of them, so without this table no published plugin's hook would ever
/// fire. Grok Build solves it the same way.
pub fn claude_aliases(horsie_tool: &str) -> &'static [&'static str] {
    match horsie_tool {
        "bash" => &["Bash"],
        "read_file" => &["Read"],
        "write_file" => &["Write"],
        // Both of horsie's in-place editors answer to Claude's edit tools.
        "find_and_replace" | "replace_lines" => &["Edit", "MultiEdit", "NotebookEdit"],
        "list_files" => &["LS"],
        "glob" => &["Glob"],
        "grep" => &["Grep"],
        // `set_env` and `set_working_dir` have no Claude equivalent.
        _ => &[],
    }
}

/// Whether a hook's `matcher` selects a tool call.
///
/// The matcher is a regex, tested unanchored against the horsie tool name and
/// each of its Claude aliases. An absent or empty matcher selects every tool.
/// A matcher that fails to compile selects nothing: a broken pattern must not
/// silently widen into "everything".
pub fn matcher_applies(matcher: Option<&str>, horsie_tool: &str) -> bool {
    let Some(pattern) = matcher.map(str::trim).filter(|m| !m.is_empty()) else {
        return true;
    };
    let Ok(re) = regex::Regex::new(pattern) else {
        return false;
    };
    re.is_match(horsie_tool)
        || claude_aliases(horsie_tool)
            .iter()
            .any(|alias| re.is_match(alias))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-support hooks`
Expected: PASS (11 tests)

- [ ] **Step 5: Commit**

```bash
git add support/src/plugin/hooks.rs
git commit -m "feat(support): Claude tool-name aliasing and matcher evaluation"
```

---

### Task 3: Reading hooks.json

**Files:**
- Modify: `support/src/plugin/hooks.rs`

**Interfaces:**
- Consumes: `HookEvent`, `Unsupported` (Task 1).
- Produces:
  ```rust
  pub struct HookDecl {
      pub event: HookEvent,
      pub matcher: Option<String>,
      pub command: String,
      pub timeout: Option<u64>,
  }
  pub struct PluginHooks {
      pub decls: Vec<HookDecl>,
      pub unsupported: Vec<(String, Unsupported)>,
  }
  pub fn read(plugin_root: &Path) -> Result<PluginHooks, String>;
  ```
  `read` returns an empty `PluginHooks` when the file is absent, and `Err` only when it exists but is malformed.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests`:

```rust
    use tempfile::TempDir;

    fn write_hooks(root: &std::path::Path, json: &str) {
        let dir = root.join("hooks");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("hooks.json"), json).unwrap();
    }

    #[test]
    fn absent_hooks_file_is_empty_not_an_error() {
        let dir = TempDir::new().unwrap();
        let h = read(dir.path()).unwrap();
        assert!(h.decls.is_empty());
        assert!(h.unsupported.is_empty());
    }

    /// impeccable's shape: matcher plus a per-hook timeout.
    #[test]
    fn reads_matcher_command_and_timeout() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{"PostToolUse":[{"matcher":"Edit|Write","hooks":[
                 {"type":"command","command":"node hook.mjs","timeout":5}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 1);
        assert_eq!(h.decls[0].event, HookEvent::PostToolUse);
        assert_eq!(h.decls[0].matcher.as_deref(), Some("Edit|Write"));
        assert_eq!(h.decls[0].command, "node hook.mjs");
        assert_eq!(h.decls[0].timeout, Some(5));
    }

    #[test]
    fn several_events_and_several_hooks_each_are_all_collected() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{
                 "PreToolUse":[{"hooks":[{"type":"command","command":"a"},
                                         {"type":"command","command":"b"}]}],
                 "Stop":[{"hooks":[{"type":"command","command":"c"}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 3);
        assert_eq!(
            h.decls.iter().filter(|d| d.event == HookEvent::PreToolUse).count(),
            2
        );
    }

    #[test]
    fn unsupported_events_are_collected_with_their_reason_not_dropped() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{
                 "PreToolUse":[{"hooks":[{"type":"command","command":"ok"}]}],
                 "PostToolBatch":[{"hooks":[{"type":"command","command":"x"}]}],
                 "WorktreeCreate":[{"hooks":[{"type":"command","command":"y"}]}],
                 "Bogus":[{"hooks":[{"type":"command","command":"z"}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 1, "only the supported event runs");
        let mut reasons: Vec<(String, Unsupported)> = h.unsupported;
        reasons.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            reasons,
            vec![
                ("Bogus".to_string(), Unsupported::Unknown),
                ("PostToolBatch".to_string(), Unsupported::NotImplemented),
                ("WorktreeCreate".to_string(), Unsupported::NoConcept),
            ]
        );
    }

    /// Only `type: "command"` hooks are runnable; anything else is ignored
    /// rather than mistaken for one.
    #[test]
    fn non_command_hooks_are_skipped() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{"PreToolUse":[{"hooks":[
                 {"type":"http","url":"https://x"},
                 {"type":"command","command":"real"}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 1);
        assert_eq!(h.decls[0].command, "real");
    }

    #[test]
    fn a_malformed_hooks_file_is_an_error_not_an_empty_set() {
        let dir = TempDir::new().unwrap();
        write_hooks(dir.path(), "{not json");
        let err = read(dir.path()).unwrap_err();
        assert!(err.contains("hooks.json"), "{err}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p horsie-support hooks`
Expected: FAIL — `read` not defined.

- [ ] **Step 3: Implement**

Insert above the `#[cfg(test)]` block:

```rust
/// One runnable hook command, already classified and located.
#[derive(Debug, Clone)]
pub struct HookDecl {
    pub event: HookEvent,
    pub matcher: Option<String>,
    pub command: String,
    /// Per-hook budget in seconds; `None` means the caller's default.
    pub timeout: Option<u64>,
}

/// Everything a plugin declares, split into what horsie can run and what it
/// cannot.
#[derive(Debug, Clone, Default)]
pub struct PluginHooks {
    pub decls: Vec<HookDecl>,
    pub unsupported: Vec<(String, Unsupported)>,
}

/// Read `<plugin_root>/hooks/hooks.json`.
///
/// An absent file is an empty set. A present but malformed file is an error —
/// silently treating it as "no hooks" is the failure mode this whole phase
/// exists to remove.
pub fn read(plugin_root: &Path) -> Result<PluginHooks, String> {
    let path = plugin_root.join("hooks").join("hooks.json");
    if !path.is_file() {
        return Ok(PluginHooks::default());
    }
    let text =
        std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let json: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("hooks.json: {e}"))?;

    let Some(events) = json.get("hooks").and_then(serde_json::Value::as_object) else {
        return Ok(PluginHooks::default());
    };

    let mut out = PluginHooks::default();
    for (name, matchers) in events {
        let event = match HookEvent::parse(name) {
            Ok(e) => e,
            Err(reason) => {
                out.unsupported.push((name.clone(), reason));
                continue;
            }
        };
        let Some(matchers) = matchers.as_array() else {
            continue;
        };
        for entry in matchers {
            let matcher = entry
                .get("matcher")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string);
            let Some(hooks) = entry.get("hooks").and_then(serde_json::Value::as_array) else {
                continue;
            };
            for hook in hooks {
                if hook.get("type").and_then(serde_json::Value::as_str) != Some("command") {
                    continue;
                }
                let Some(command) = hook.get("command").and_then(serde_json::Value::as_str) else {
                    continue;
                };
                out.decls.push(HookDecl {
                    event,
                    matcher: matcher.clone(),
                    command: command.to_string(),
                    timeout: hook.get("timeout").and_then(serde_json::Value::as_u64),
                });
            }
        }
    }
    Ok(out)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p horsie-support hooks`
Expected: PASS (17 tests)

- [ ] **Step 5: Commit**

```bash
git add support/src/plugin/hooks.rs
git commit -m "feat(support): read and classify plugin hooks.json"
```

---

### Task 4: Protocol messages

**Files:**
- Modify: `models/fluorite/runtime.fl`

**Interfaces:**
- Produces (via codegen, as `horsie_models::runtime::*`):
  `HookDeclWire`, `HookManifestRequest`, `HookManifestResponse`, `RunHookRequest`,
  `HookOutcomeWire`, `RunHookResponse`, and two new variants on each of
  `RuntimeInboundMessage` / `RuntimeOutboundMessage`.

- [ ] **Step 1: Add the messages**

In `models/fluorite/runtime.fl`, replace the `// --- SessionStart hooks ---` section header and append after `SessionStartResponse`:

```
// --- Plugin hooks ---

/// One hook a plugin declares, as advertised to the server. The command itself
/// never crosses the wire: it runs runtime-side, where the plugin files are.
struct HookDeclWire { event: String, matcher: Option<String> }

/// Ask the runtime what hooks the session's plugins declare.
///
/// Answering this is also how a runtime announces that it supports hooks at
/// all: the protocol carries no version, so a runtime that cannot answer is
/// treated as hook-less and the server falls back to `SessionStart` alone.
struct HookManifestRequest  { call_id: String }
struct HookManifestResponse {
    call_id: String,
    entries: Vec<HookDeclWire>,
    /// Event names the plugins declared that horsie cannot run, each already
    /// rendered as a full sentence naming the event and why.
    unsupported: Vec<String>,
}

/// Run every hook matching `event`, in stable plugin order.
struct RunHookRequest {
    call_id: String,
    event: String,
    /// The event payload, JSON-encoded, fed to each hook on stdin.
    payload: String,
}

/// The merged result of every hook that ran for one event.
struct HookOutcomeWire {
    /// A hook blocked the action: exit 2, `decision: "block"`, or
    /// `permissionDecision: "deny"`.
    blocked: bool,
    reason: Option<String>,
    additional_context: Option<String>,
    /// Replacement tool input and output, JSON-encoded.
    updated_input: Option<String>,
    updated_tool_output: Option<String>,
    system_message: Option<String>,
    /// `continue: false` — the caller should end the turn.
    stop: bool,
    stop_reason: Option<String>,
    /// A hook could not be run to completion: spawn failure, timeout, or a
    /// non-zero exit other than 2. Distinct from `blocked` because the caller
    /// treats intent and outage differently — `PreToolUse` denies on both, and
    /// every other event proceeds on a failure.
    failed: bool,
}

struct RunHookResponse { call_id: String, outcome: HookOutcomeWire }
```

Add to `RuntimeInboundMessage`:

```
    HookManifest(HookManifestRequest),
    RunHook(RunHookRequest),
```

Add to `RuntimeOutboundMessage`:

```
    HookManifestResult(HookManifestResponse),
    RunHookResult(RunHookResponse),
```

- [ ] **Step 2: Verify codegen compiles**

Run: `cargo build -p horsie-models`
Expected: success.

- [ ] **Step 3: Fix the exhaustive matches this breaks**

`RuntimeOutboundMessage` is matched exhaustively in `runtime-client/src/transport.rs` (`scan_workspace`, `run_session_start`) — the workspace lints deny `wildcard_enum_match_arm`, so each match needs the two new variants added to its error arm. Build the workspace and add them where the compiler points:

Run: `cargo build --workspace 2>&1 | grep -A5 "non-exhaustive\|not covered"`

- [ ] **Step 4: Commit**

```bash
git add models/fluorite/runtime.fl runtime-client/ Cargo.lock
git commit -m "feat(models): hook manifest and RunHook protocol messages"
```

---

### Task 5: Transport and client methods

**Files:**
- Modify: `runtime-client/src/transport.rs`, `runtime-client/src/client.rs`, `runtime-client/src/testkit.rs`

**Interfaces:**
- Consumes: the Task 4 message types.
- Produces:
  ```rust
  // RuntimeTransport (default methods)
  async fn hook_manifest(&self, call_id: &str)
      -> Result<HookManifestResponse, TransportError>;
  async fn run_hook(&self, call_id: &str, event: &str, payload: &str)
      -> Result<HookOutcomeWire, TransportError>;

  // RuntimeClient
  pub async fn hook_manifest(&self) -> Result<HookManifestResponse, RuntimeCallError>;
  pub async fn run_hook(&self, event: &str, payload: &str)
      -> Result<HookOutcomeWire, RuntimeCallError>;
  ```

- [ ] **Step 1: Add the transport defaults**

In `runtime-client/src/transport.rs`, after `run_session_start`, following its shape exactly:

```rust
    /// What hooks the session's plugins declare. A runtime that does not
    /// implement this message is treated as hook-less by the caller.
    async fn hook_manifest(
        &self,
        call_id: &str,
    ) -> Result<HookManifestResponse, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::HookManifest(HookManifestRequest {
                call_id: call_id.to_string(),
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::HookManifestResult(resp) => Ok(resp),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ToolCallResponse(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::SessionStartResult(_)
            | RuntimeOutboundMessage::RunHookResult(_) => Err(wrong_reply("a hook manifest")),
        }
    }

    /// Run every hook matching `event` and return their merged outcome.
    async fn run_hook(
        &self,
        call_id: &str,
        event: &str,
        payload: &str,
    ) -> Result<HookOutcomeWire, TransportError> {
        let reply = self
            .relay(RuntimeInboundMessage::RunHook(RunHookRequest {
                call_id: call_id.to_string(),
                event: event.to_string(),
                payload: payload.to_string(),
            }))
            .await?;
        match reply {
            RuntimeOutboundMessage::RunHookResult(resp) => Ok(resp.outcome),
            RuntimeOutboundMessage::Ready(_)
            | RuntimeOutboundMessage::Provisioning(_)
            | RuntimeOutboundMessage::ProvisionFailed(_)
            | RuntimeOutboundMessage::ToolCallResponse(_)
            | RuntimeOutboundMessage::ScanResult(_)
            | RuntimeOutboundMessage::SessionStartResult(_)
            | RuntimeOutboundMessage::HookManifestResult(_) => Err(wrong_reply("a hook run")),
        }
    }
```

Extend the `use horsie_models::runtime::{…}` list with `HookManifestRequest`, `HookManifestResponse`, `HookOutcomeWire`, `RunHookRequest`.

- [ ] **Step 2: Add the client wrappers**

In `runtime-client/src/client.rs`, mirroring `run_session_start`:

```rust
    /// What hooks the session's plugins declare.
    pub async fn hook_manifest(&self) -> Result<HookManifestResponse, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.inner.hook_manifest(&call_id).await.map_err(|e| {
            RuntimeCallError::Transport(e.to_string())
        })
    }

    /// Run every hook matching `event`; `payload` is the event's JSON body.
    pub async fn run_hook(
        &self,
        event: &str,
        payload: &str,
    ) -> Result<HookOutcomeWire, RuntimeCallError> {
        let call_id = Uuid::new_v4().to_string();
        self.inner
            .run_hook(&call_id, event, payload)
            .await
            .map_err(|e| RuntimeCallError::Transport(e.to_string()))
    }
```

Match the exact `RuntimeCallError` construction used by `run_session_start` in that file; if it wraps differently, follow it rather than the sketch above.

- [ ] **Step 3: Teach the testkit to answer**

In `runtime-client/src/testkit.rs`, beside the existing `RuntimeInboundMessage::SessionStart` arm, add arms answering `HookManifest` with a configurable manifest (default: empty) and `RunHook` with a configurable outcome (default: all-false/none). Add builder methods `with_hook_manifest(entries, unsupported)` and `with_hook_outcome(outcome)` following the existing `Override the canned SessionStart context` pattern.

- [ ] **Step 4: Build and run the client tests**

Run: `cargo test -p horsie-runtime-client`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add runtime-client/
git commit -m "feat(runtime-client): hook manifest and run_hook calls"
```

---

### Task 6: Runtime-side manifest and executor

**Files:**
- Create: `runtime/src/hooks.rs`
- Modify: `runtime/src/lib.rs`, `runtime/src/main.rs`, `runtime/src/plugins.rs`

**Interfaces:**
- Consumes: `horsie_support::plugin::hooks::{read, HookEvent, HookDecl}`.
- Produces:
  ```rust
  pub fn manifest(plugins_dir: &Path) -> (Vec<HookDeclWire>, Vec<String>);
  pub async fn run(plugins_dir: &Path, hook_path: &[PathBuf], event: &str, payload: &str)
      -> HookOutcomeWire;
  ```

- [ ] **Step 1: Write the failing tests**

Create `runtime/src/hooks.rs` with a `mod tests` covering: a plugin declaring `PreToolUse` appears in the manifest with its matcher; an unsupported event appears in `unsupported` as an explanatory sentence; a hook exiting 0 with `{"hookSpecificOutput":{"additionalContext":"X"}}` yields `additional_context == Some("X")`; a hook exiting 2 with stderr yields `blocked` with that reason; a hook exiting 1 yields `failed` and **not** `blocked`; a hook printing `{"hookSpecificOutput":{"permissionDecision":"deny","permissionDecisionReason":"no"}}` yields `blocked`; `permissionDecision: "ask"` yields neither `blocked` nor `failed`; `updatedInput` and `updatedToolOutput` round-trip as JSON strings; two matching hooks concatenate their `additionalContext` and the first `blocked` stops the chain. Build fixtures by writing tiny `sh` scripts into a `TempDir` plugin tree.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-runtime hooks`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement `manifest`**

```rust
/// What the session's plugins declare, and what horsie cannot run.
///
/// Reading fails loudly per plugin: a malformed `hooks.json` is reported as an
/// unsupported entry rather than silently yielding no hooks.
pub fn manifest(plugins_dir: &Path) -> (Vec<HookDeclWire>, Vec<String>) {
    let mut entries = Vec::new();
    let mut unsupported = Vec::new();
    for plugin_root in crate::plugins::plugin_dirs(plugins_dir) {
        let name = plugin_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        match horsie_support::plugin::hooks::read(&plugin_root) {
            Ok(hooks) => {
                for d in hooks.decls {
                    entries.push(HookDeclWire {
                        event: d.event.as_str().to_string(),
                        matcher: d.matcher,
                    });
                }
                for (event, why) in hooks.unsupported {
                    unsupported.push(format!("plugin '{name}': {}", why.explain(&event)));
                }
            }
            Err(e) => unsupported.push(format!("plugin '{name}': {e}")),
        }
    }
    (entries, unsupported)
}
```

Make `plugins::plugin_dirs` `pub(crate)` if it is private.

- [ ] **Step 4: Implement `run` and the control protocol**

`run` collects every `HookDecl` whose `event` matches, in stable plugin order, and for each one invokes the existing `run_hook` process machinery in `runtime/src/plugins.rs` — reusing its cwd, `CLAUDE_PLUGIN_ROOT`, `PATH` and output-clamp behaviour, but returning the raw exit status, stdout and stderr rather than only injected context. Refactor `plugins::run_hook` to return `(ExitStatus-ish, String stdout, String stderr)` and have the existing `run_session_start` keep its current behaviour on top of that.

Parsing, per hook:

```rust
/// Interpret one hook's exit status and stdout per Claude Code's contract.
///
/// exit 0 → stdout parsed as JSON when it parses, else treated as
/// `additionalContext`. exit 2 → blocking, stderr is the reason. Anything else
/// (including timeout and spawn failure) → `failed`.
fn interpret(code: Option<i32>, stdout: &str, stderr: &str, out: &mut HookOutcomeWire) {
    match code {
        Some(0) => merge_json(stdout, out),
        Some(2) => {
            out.blocked = true;
            out.reason = Some(stderr.trim().to_string()).filter(|s| !s.is_empty());
        }
        _ => out.failed = true,
    }
}
```

`merge_json` reads `continue`/`stopReason`/`systemMessage`, top-level `decision == "block"` with `reason`, and `hookSpecificOutput` with `additionalContext`, `permissionDecision`, `permissionDecisionReason`, `updatedInput`, `updatedToolOutput`. `permissionDecision` of `"deny"` sets `blocked`; `"ask"` and `"defer"` are logged via `tracing::info!` and otherwise ignored, matching the spec. `additionalContext` accumulates with `\n\n` between hooks; `updated_input`/`updated_tool_output` overwrite. The loop breaks on the first `blocked`.

- [ ] **Step 5: Dispatch the messages**

In `runtime/src/main.rs`, beside the existing `RuntimeInboundMessage::SessionStart` arm, add arms for `HookManifest` and `RunHook` that call `hooks::manifest` / `hooks::run` with the registry's `plugins_dir()` and `hook_path()`, answering with `HookManifestResult` / `RunHookResult`. When `plugins_dir()` is `None`, answer with an empty manifest and an all-default outcome.

- [ ] **Step 6: Run the tests**

Run: `cargo test -p horsie-runtime`
Expected: PASS, including the pre-existing `SessionStart` tests unmodified.

- [ ] **Step 7: Commit**

```bash
git add runtime/
git commit -m "feat(runtime): hook manifest and event execution with the control protocol"
```

---

### Task 7: The HookedToolbox decorator

**Files:**
- Create: `server/src/sessions/hooked_toolbox.rs`
- Modify: `server/src/sessions/mod.rs`

**Interfaces:**
- Consumes: `matcher_applies` (Task 2), `RuntimeClient::run_hook` (Task 5), `HookDeclWire`.
- Produces:
  ```rust
  pub struct HookedToolbox { /* inner, client, decls */ }
  impl HookedToolbox {
      pub fn new(inner: Arc<dyn Toolbox>, client: RuntimeClient,
                 decls: Vec<HookDeclWire>) -> Self;
      /// `None` when no declaration targets a tool event — callers skip wrapping.
      pub fn wrap(inner: Arc<dyn Toolbox>, client: RuntimeClient,
                  decls: Vec<HookDeclWire>) -> Arc<dyn Toolbox>;
  }
  ```

- [ ] **Step 1: Write the failing tests**

Create `server/src/sessions/hooked_toolbox.rs` with a `mod tests` using a stub `Toolbox` recording whether it was called and a stub runtime transport returning canned outcomes. Cover: with no matching declaration the inner toolbox is called and **no** `run_hook` round-trip happens; a `blocked` `PreToolUse` outcome means the inner toolbox is never called and the error text carries the reason; a `failed` `PreToolUse` outcome also denies (fail closed); `updated_input` replaces what the inner toolbox receives; `updated_tool_output` replaces what the caller gets back; a `failed` `PostToolUse` outcome leaves the tool's real output intact; `additional_context` from `PostToolUse` is appended to the output the model sees.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p horsie-server hooked_toolbox`
Expected: FAIL — module not found.

- [ ] **Step 3: Implement**

```rust
//! `Toolbox` decorator that runs `PreToolUse` and `PostToolUse` hooks.
//!
//! This is the whole of tool-hook support: `agentcore`'s loop dispatches every
//! tool through `Toolbox::execute`, so wrapping the box is enough and the loop
//! never learns hooks exist.

#[async_trait]
impl Toolbox for HookedToolbox {
    fn specs(&self) -> Vec<ToolSpec> {
        self.inner.specs()
    }

    async fn execute(&self, name: &str, input: Value) -> Result<Value, ToolCallError> {
        let mut input = input;
        if self.matches(HookEvent::PreToolUse, name) {
            let payload = json!({
                "hook_event_name": "PreToolUse",
                "tool_name": name,
                "tool_input": input,
            });
            let outcome = self.run("PreToolUse", &payload).await;
            // Fail closed: a guard that could not run is not a guard. This is a
            // deliberate divergence from Claude Code, and applies to
            // `PreToolUse` alone — every other event runs after the fact.
            if outcome.blocked || outcome.failed {
                return Err(ToolCallError::ExecutionFailed(deny_reason(&outcome, name)));
            }
            if let Some(updated) = outcome.updated_input.as_deref()
                && let Ok(v) = serde_json::from_str(updated)
            {
                input = v;
            }
        }

        let result = self.inner.execute(name, input.clone()).await;

        if self.matches(HookEvent::PostToolUse, name) {
            let (output, is_error) = match &result {
                Ok(v) => (v.clone(), false),
                Err(e) => (Value::String(e.to_string()), true),
            };
            let payload = json!({
                "hook_event_name": "PostToolUse",
                "tool_name": name,
                "tool_input": input,
                "tool_response": output,
                "is_error": is_error,
            });
            let outcome = self.run("PostToolUse", &payload).await;
            // PostToolUse runs after the fact: a failure here is logged, never
            // fatal, and never rewrites a result it could not read.
            if !outcome.failed && let Ok(v) = result.as_ref() {
                return Ok(apply_post(v.clone(), &outcome));
            }
        }
        result
    }
}
```

`matches` tests `matcher_applies(decl.matcher, name)` over the declarations for that event, so a session with no tool declarations never round-trips. `deny_reason` names the tool and, when the outcome merely `failed`, says the hook could not be run. `apply_post` replaces the value with `updated_tool_output` when present and appends `additional_context` to a string output. `wrap` returns the inner box unchanged when no declaration has a `PreToolUse` or `PostToolUse` event, so the decorator is not even constructed for hook-less sessions.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-server hooked_toolbox`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add server/src/sessions/
git commit -m "feat(server): HookedToolbox running PreToolUse and PostToolUse"
```

---

### Task 8: Session wiring and the session-start gate

**Files:**
- Modify: `server/src/sessions/session_actor.rs`

- [ ] **Step 1: Fetch the manifest and validate**

At `session_actor.rs`, in the `use_plugins` branch that already calls
`run_session_start` (around line 745), first call `runtime_client.hook_manifest()`:

- `Err(_)` → the runtime does not support hooks. Record an empty declaration
  list and continue exactly as today. This is the version-negotiation fallback:
  an older `horsie connect` keeps working.
- `Ok(m)` with a non-empty `m.unsupported` → `tracing::warn!` each sentence.
  PR2 turns this into a hard failure, once the five events it wires stop
  appearing in that list.
- `Ok(m)` → keep `m.entries` for the toolbox.

- [ ] **Step 2: Wrap the toolbox**

Where the agent's toolbox is built for the session, wrap it with
`HookedToolbox::wrap(inner, runtime_client.clone(), entries)`. `wrap` returns
the inner box untouched when no entry targets a tool event, so hook-less
sessions are unaffected.

- [ ] **Step 3: Add a session-level test**

Add a test asserting that a manifest error — an older runtime that does not
implement the message — leaves session start working exactly as it does today,
with no hooks and no failure. This is the version-negotiation path, and it is
the one that silently breaks every `horsie connect` user if it regresses.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p horsie-server sessions`
Expected: PASS

- [ ] **Step 5: Commit**

```bash
git add server/src/sessions/session_actor.rs
git commit -m "feat(server): fetch the hook manifest and wrap the session toolbox"
```

---

### Task 9: Docs, gates, live verification, PR

**Files:**
- Modify: `docs/guide/skills-and-plugins.md`

- [ ] **Step 1: Document hook support**

Add a "Hooks" section after "Where horsie looks for skills": that horsie runs
`PreToolUse` and `PostToolUse` hooks (plus `SessionStart` as before); that
matchers written for Claude Code work because Claude's tool names are aliased to
horsie's; that a `PreToolUse` hook which cannot run denies the tool; and that
hooks run with the runtime's privileges — unsandboxed on a default
`horsie connect`. Note that other events are recognised but not yet run.

- [ ] **Step 2: Run the gates in order**

```bash
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace
```

Expected: clean. Do not open a PR on red.

- [ ] **Step 3: Verify against a real plugin**

Install impeccable, which declares a `PostToolUse` hook with an
`Edit|Write|MultiEdit|NotebookEdit` matcher and a `Stop` hook:

```bash
cargo run -p horsie -- plugin install https://github.com/pbakaus/impeccable --config <tmp>
```

Expected: **installs**, exactly as it does on main — PR1 adds no gate. Confirm
`horsie plugin list` shows it, proving this PR does not regress what Phase 0
delivered. Its `PostToolUse` hook is now eligible to fire on `write_file` and
`find_and_replace`; its `Stop` hook is recognised and warned about, and starts
firing in PR2.

- [ ] **Step 4: Push and open the PR**

Body: one long line per paragraph or bullet. State that the agent loop is
untouched, that matchers are aliased onto horsie tool names, the two documented
divergences (fail-closed `PreToolUse`, `ask` treated as allow), and that
impeccable is currently rejected until PR2 lands `Stop`.

---

## Self-Review

**Spec coverage:**

| Spec section | Task |
| --- | --- |
| Event inventory; all 31 classified | 1 |
| Tool-name aliasing, matcher regex, server-side evaluation | 2, 7 |
| Hook reader, per-hook timeout, `Unsupported` reasons | 1, 3 |
| Protocol: manifest, `RunHook`, `HookOutcomeWire` | 4, 5 |
| Runtime executor, control protocol, exit codes | 6 |
| Fail-closed `PreToolUse`; `ask`/`defer` as allow | 6, 7 |
| `updatedInput` / `updatedToolOutput` | 6, 7 |
| Manifest as optimisation (no round-trip without a match) | 7 (`wrap` + `matches`) |
| Manifest as negotiation (old runtime → hook-less) | 8 |
| Multiple hooks: order, concatenation, first-block-wins | 6 |
| Docs | 9 |

**Deliberately deferred to PR2**, and the reason each is safe to defer:

- **`SessionEnd`, `UserPromptSubmit`, `Stop`, `SubagentStart`, `SubagentStop`.**
  They classify as `NotImplemented` here rather than supported, so no plugin can
  install believing they work. Promoting them in PR2 is a one-line change per
  event plus its call site, and the counts test in Task 1 forces that change to
  be deliberate.
- **The install and session-start failure gates.** Landing them in PR1 would
  reject impeccable, which declares `Stop` — un-shipping what Phase 0 delivered
  to make it installable. PR1 warns where PR2 will fail, so the behaviour for
  unsupported events is unchanged from main rather than regressed. PR2 adds both
  gates in the same change that removes the five legitimate reasons to trip them.

This is the one place PR1 is knowingly quieter than the spec's end state, and it
is a sequencing choice rather than a scope cut: the end state after PR2 is
exactly what the spec describes.

**Type consistency:** `HookEvent`, `Unsupported`, `HookDecl` and `PluginHooks`
are defined in Tasks 1 and 3 and consumed under those names in 6 and 7.
`HookOutcomeWire` field names match between the `.fl` in Task 4 and every use in
5, 6 and 7. `matcher_applies(Option<&str>, &str) -> bool` has one signature,
used in Tasks 2 and 7. `hook_manifest` and `run_hook` appear with identical
signatures in Tasks 5, 7 and 8. `HookEvent::as_str` covers exactly the three
variants the enum declares, so adding a PR2 variant fails to compile until its
name is added — which is the intended forcing function.

**Placeholders:** Tasks 6, 7 and 8 describe some test bodies and wiring by
contract rather than transcribing every line, because the fixtures follow
existing patterns in `runtime/src/plugins.rs` and `server/src/sessions/`
verbatim; every new type, signature, control-flow decision and non-obvious body
is given in full.
