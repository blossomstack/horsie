# Implementation plan: shared plugins

Spec: [../specs/2026-06-01-shared-plugins-design.md](../specs/2026-06-01-shared-plugins-design.md)

Bottom-up, each phase compiles + tests green before the next. Commit per phase.

## Phase 1 — Protocol types (fluorite + models)
- `fluorite/workflow.fl`: `WorkflowAgentDef.use_plugins: Option<bool>`,
  `WorkflowDefinition.default_use_plugins: Option<bool>`.
- `fluorite/runtime.fl`: `ScanRequest.include_shared: Bool`; `PluginSkill { plugin, rel_dir, content }`;
  `ScanResponse.shared_skills: Vec<PluginSkill>`; `SessionStartRequest`/`SessionStartResponse`
  + arms in `RuntimeInboundMessage`/`RuntimeOutboundMessage`.
- `fluorite/executor.fl`: `RuntimeConfig.plugins_dir: Option<String>`, `hook_path: Vec<String>`.
- Regen: `cargo build -p models`. Fix all downstream constructors to compile.
- **Verify:** `cargo build --workspace`.

## Phase 2 — Config + capabilities + interpreter resolution (cli)
- `cli/src/config.rs`: `StorageConfig.plugins_dir` (default `<data_dir>/plugins`);
  `RuntimeConfig.hook_path: Option<Vec<PathBuf>>`.
- New `cli/src/plugins.rs`: lockfile types + `installed_plugins(dir)` count helper;
  `resolve_hook_path(cfg)` (override else `which node`, pure-testable core).
- `cli/src/capabilities.rs`: add Read `DirGrant` for plugins_dir (if populated) + each hook dir.
- **Verify:** `cargo test -p cli`.

## Phase 3 — Runtime plugin engine (runtime)
- `runtime/src/plugins.rs` (new): enumerate plugins, parse `.claude-plugin/plugin.json`
  (name/version/skills override), discover skills (`PluginSkill` w/ rel_dir), collect+run
  `SessionStart` hooks (cwd, `CLAUDE_PLUGIN_ROOT`, PATH prepend, stdin, timeout, envelope vs raw).
- `runtime/src/workspace.rs`: reserved read-only `horsie_shared` registry entry (excluded from
  single-default count); reserved-name validation in `derive_workspaces`/registry build.
- `runtime/src/scan.rs`: honor `include_shared` → fill `shared_skills`.
- `runtime/src/main.rs`: `--plugins-dir`, `--hook-path` args; wire registry + handlers
  (`ScanWorkspace` fills shared, new `SessionStart` arm).
- **Verify:** `cargo test -p runtime`.

## Phase 4 — Runtime client
- `runtime-client`: `scan_workspace(..., include_shared)`; new `run_session_start()`;
  `MockTransport` support (`with_shared`, `with_session_context`).
- **Verify:** `cargo test -p runtime-client`.

## Phase 5 — Workflow integration
- `workflow/src/workspace.rs`: `SharedSkillSet`/`SharedContext`; `Skill.rel_dir`;
  parse `shared_skills`; `compose_system_prompt(..., shared)` → `# Session bootstrap` + `# Shared skills`.
- `workflow/src/context.rs`: toolbox learns `use_plugins` + `horsie_shared`; `skill` resource hint;
  `inspect_workspace` shared section.
- `workflow/src/workflow_actor.rs`: `effective_use_plugins`; scan with `include_shared`;
  call `run_session_start`; thread `SharedContext`.
- **Verify:** `cargo test -p workflow`.

## Phase 6 — Plumbing (cli plugin cmds, executor, supervisor, daemon)
- `cli/src/main.rs`: `Plugin` command group (`install`/`list`/`update`/`remove`); pass
  plugins_dir + resolved hook_path into submit.
- `supervisor/src/spec.rs`: `JobSpec.plugins_dir`/`hook_path`; thread to `RuntimeConfig`.
- `cli/src/daemon/mod.rs` + `executor/src/process_provider.rs`: `--plugins-dir`/`--hook-path`.
- **Verify:** `cargo build --workspace`.

## Phase 7 — e2e + green
- e2e under `cli`/`workflow` `tests/`: fixture plugin (skill + reference + echo SessionStart hook),
  opted-in vs opted-out agents.
- **Verify:** `cargo clippy --all-targets --all-features -- -D warnings`, `cargo fmt --check`,
  `cargo test --workspace`.
