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
    HookEvent, OutputField, Unsupported, claude_aliases, horsie_tools_for, matcher_applies,
    matcher_selects,
};
pub use invoke::HookInvocation;
// `HookDecl`, `HookTransport` and `PluginHooks` are declared below.
pub use process::{Halt, HookOutput, HookReply, Permission, Verdict, process};

use std::path::Path;

/// How a hook is invoked.
///
/// The two the spec defines. Kept as a sum type rather than an optional URL
/// beside an optional command so a declaration cannot be both or neither, and
/// so the runtime's dispatch is an exhaustive match.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HookTransport {
    /// `type: "command"` — a shell command, run with the plugin root as cwd.
    Command(String),
    /// `type: "http"` — the payload POSTed as a JSON body, the response body
    /// read as the reply. There is no exit-code channel, so an HTTP hook can
    /// only block through `decision` / `permissionDecision` in that body.
    Http {
        url: String,
        /// Extra request headers, sorted by name — `serde_json`'s object is a
        /// `BTreeMap` without the `preserve_order` feature, so declaration
        /// order is not recoverable and nothing may depend on it.
        headers: Vec<(String, String)>,
        /// `allowedEnvVars` — the environment variables a header value may
        /// interpolate. An allowlist rather than free substitution: a header is
        /// where a plugin puts a credential, and a hook that could name any
        /// variable could exfiltrate every one the runtime holds.
        allowed_env_vars: Vec<String>,
    },
}

/// One runnable hook, already classified and located.
#[derive(Debug, Clone)]
pub struct HookDecl {
    pub event: HookEvent,
    pub matcher: Option<String>,
    pub transport: HookTransport,
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
                let Some(transport) = transport_of(hook) else {
                    continue;
                };
                out.decls.push(HookDecl {
                    event,
                    matcher: matcher.clone(),
                    transport,
                    timeout: hook.get("timeout").and_then(serde_json::Value::as_u64),
                });
            }
        }
    }
    Ok(out)
}

/// One `hooks[]` entry's transport, or `None` when it declares neither shape
/// completely — a `command` hook with no command runs nothing, and an `http`
/// hook with no url has nowhere to go.
fn transport_of(hook: &serde_json::Value) -> Option<HookTransport> {
    let str_at = |k: &str| hook.get(k).and_then(serde_json::Value::as_str);
    match hook.get("type").and_then(serde_json::Value::as_str) {
        Some("command") => Some(HookTransport::Command(str_at("command")?.to_string())),
        Some("http") => Some(HookTransport::Http {
            url: str_at("url")?.to_string(),
            headers: hook
                .get("headers")
                .and_then(serde_json::Value::as_object)
                .map(|h| {
                    h.iter()
                        .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_string())))
                        .collect()
                })
                .unwrap_or_default(),
            allowed_env_vars: hook
                .get("allowedEnvVars")
                .and_then(serde_json::Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(|v| Some(v.as_str()?.to_string()))
                        .collect()
                })
                .unwrap_or_default(),
        }),
        // An unknown type is skipped rather than guessed at: a future transport
        // must be added here deliberately.
        _ => None,
    }
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
        assert_eq!(
            h.decls[0].transport,
            HookTransport::Command("node hook.mjs".into())
        );
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

    /// Both declared transports are read. This used to skip the `http` one,
    /// which is what made a webhook-only plugin install to silence.
    #[test]
    fn command_and_http_hooks_are_both_read() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{"PreToolUse":[{"hooks":[
                 {"type":"http","url":"https://x","headers":{"X-Key":"s"}},
                 {"type":"command","command":"real"}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(h.decls.len(), 2);
        assert_eq!(
            h.decls[0].transport,
            HookTransport::Http {
                url: "https://x".into(),
                headers: vec![("X-Key".into(), "s".into())],
                allowed_env_vars: Vec::new(),
            }
        );
        assert_eq!(h.decls[1].transport, HookTransport::Command("real".into()));
    }

    /// `allowedEnvVars` is read, because a header is where a plugin puts its
    /// credential. Dropped, the `$TOKEN` in the value went out literally and
    /// the endpoint answered 401 — which `PreToolUse` then failed closed on.
    #[test]
    fn an_http_hook_declares_the_env_vars_its_headers_may_read() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"http","url":"https://x",
                 "headers":{"Authorization":"Bearer $MY_TOKEN"},
                 "allowedEnvVars":["MY_TOKEN"]}]}]}}"#,
        );
        let h = read(dir.path()).unwrap();
        assert_eq!(
            h.decls[0].transport,
            HookTransport::Http {
                url: "https://x".into(),
                headers: vec![("Authorization".into(), "Bearer $MY_TOKEN".into())],
                allowed_env_vars: vec!["MY_TOKEN".into()],
            }
        );
    }

    /// A transport horsie does not know, and a declaration missing the field
    /// its own type requires, both run nothing rather than being guessed at.
    #[test]
    fn an_unknown_or_incomplete_transport_is_skipped() {
        let dir = TempDir::new().unwrap();
        write_hooks(
            dir.path(),
            r#"{"hooks":{"PreToolUse":[{"hooks":[
                 {"type":"carrier-pigeon","command":"coo"},
                 {"type":"http"},
                 {"type":"command"}]}]}}"#,
        );
        assert!(read(dir.path()).unwrap().decls.is_empty());
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
