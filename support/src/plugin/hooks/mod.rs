//! Reading and classifying `<plugin>/hooks/hooks.json`.
//!
//! The library below is spec-derived and horsie-free: it describes every hook
//! event horsie has a seam for, turns a hook process's reply into one typed
//! outcome, and folds that outcome into the event's own record. What a verdict
//! *means* is never decided here — `PreToolUse` fails closed, `Stop` blocking
//! continues a turn, `Notification` cannot block at all. One parser, three
//! consequences, and the consequences belong to the call sites.

mod events;
mod invoke;
mod process;

pub use events::{
    HookEvent, OutputField, Unsupported, claude_aliases, matcher_applies, matcher_selects,
};
pub use invoke::HookInvocation;
pub use process::{HookOutput, HookReply, Permission, Verdict, process};

use std::path::Path;

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
/// silently treating it as "no hooks" is the failure mode this whole feature
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
            // Described, but horsie has nowhere to fire it. Deferred rather
            // than accepted so no hook installs believing it works and then
            // silently never fires.
            Ok(e) if !e.is_wired() => {
                out.unsupported
                    .push((name.clone(), Unsupported::NotImplemented));
                continue;
            }
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

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn write_hooks(root: &Path, json: &str) {
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
                 "SessionStart":[{"hooks":[{"type":"command","command":"c"}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 3);
        assert_eq!(
            h.decls
                .iter()
                .filter(|d| d.event == HookEvent::PreToolUse)
                .count(),
            2
        );
    }

    /// `read()` is where horsie's capability is enforced: a plugin declaring a
    /// described-but-unwired event is told so at install, rather than
    /// installing to silence.
    #[test]
    fn read_defers_a_described_but_unwired_event() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{
                 "PreToolUse":[{"hooks":[{"type":"command","command":"ok"}]}],
                 "PostToolBatch":[{"hooks":[{"type":"command","command":"x"}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 1, "only the wired event runs");
        assert_eq!(
            h.unsupported,
            vec![("PostToolBatch".to_string(), Unsupported::NotImplemented)]
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
        let mut reasons = h.unsupported;
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

    /// impeccable's real `hooks/hooks.json`, verbatim. It is the plugin that
    /// motivated all of #105, so its exact shape is worth pinning — and it is
    /// why `Stop` is worth wiring: both of impeccable's events now run.
    #[test]
    fn impeccables_real_hooks_are_both_runnable_now() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{
                 "PostToolUse":[{"matcher":"Edit|Write|MultiEdit","hooks":[
                   {"type":"command","command":"node hook.mjs","timeout":5}]}],
                 "Stop":[{"hooks":[
                   {"type":"command","command":"node hook.mjs","timeout":30}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();

        assert_eq!(h.decls.len(), 2);
        assert!(h.unsupported.is_empty(), "{:?}", h.unsupported);

        // The matcher must reach horsie's editors and nothing else.
        let post = h
            .decls
            .iter()
            .find(|d| d.event == HookEvent::PostToolUse)
            .unwrap();
        let m = post.matcher.as_deref();
        assert!(matcher_applies(m, "write_file"));
        assert!(matcher_applies(m, "find_and_replace"));
        assert!(!matcher_applies(m, "bash"));
        assert!(!matcher_applies(m, "read_file"));
    }

    #[test]
    fn a_malformed_hooks_file_is_an_error_not_an_empty_set() {
        let dir = TempDir::new().unwrap();
        write_hooks(dir.path(), "{not json");
        let err = read(dir.path()).unwrap_err();
        assert!(err.contains("hooks.json"), "{err}");
    }
}
