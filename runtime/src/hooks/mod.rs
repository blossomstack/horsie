//! Running plugin hooks, which happens here and nowhere else.
//!
//! The runtime is the only process where the plugin files exist — every
//! vendor's job ends at materialising `plugins_dir` — so it runs both the tool
//! hooks that wrap a call it is already handling and the events the server
//! initiates over `RunHooks`. What each hook did rides back as a `HookRecord`
//! so the server can journal it and the user can see what a plugin changed.
//!
//! Nothing here parses a hook's reply: `horsie_support::plugin::hooks` owns
//! that. This owns process execution and the plugin scan.

mod server;
mod tool;

pub use server::run_hooks;
pub use tool::dispatch_with_hooks;

use horsie_models::hooks::HookRecord;
use horsie_support::plugin::hooks::{
    HookDecl, HookEvent, HookInvocation, HookOutput, HookReply, HookTransport, matcher_selects,
    process,
};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Default per-hook budget when a declaration does not set one.
pub(crate) const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// Every declaration for `event` whose matcher selects `subjects`, with the
/// plugin root and name it came from, in stable plugin order.
pub(crate) fn matching(
    plugins_dir: &Path,
    event: HookEvent,
    subjects: &[&str],
) -> Vec<(PathBuf, String, HookDecl)> {
    let mut out = Vec::new();
    for plugin_root in crate::plugins::plugin_dirs(plugins_dir) {
        let Ok(hooks) = horsie_support::plugin::hooks::read(&plugin_root) else {
            continue;
        };
        let name = plugin_root
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        for decl in hooks.decls {
            if decl.event == event && matcher_selects(decl.matcher.as_deref(), subjects) {
                out.push((plugin_root.clone(), name.clone(), decl));
            }
        }
    }
    out
}

/// Run one hook and fold its reply into an outcome and a record.
///
/// The reply is interpreted by the library, against the invocation's own event,
/// so this function holds no per-event knowledge at all — which is what lets
/// the server-initiated path reuse it verbatim.
pub(crate) async fn run_one(
    plugin_root: &Path,
    plugin: &str,
    decl: &HookDecl,
    hook_path: &[PathBuf],
    invocation: HookInvocation<'_>,
) -> (HookOutput, HookRecord) {
    let root = plugin_root.to_string_lossy();
    let expand = |s: &str| s.replace("${CLAUDE_PLUGIN_ROOT}", &root);
    let timeout = decl.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs);
    let payload = invocation.payload();

    let started = Instant::now();
    // Both transports produce the same `HookRun`, so everything below — the
    // reply processor, the record, the clamp — is shared. A hook's transport is
    // its plumbing, never part of what it decided.
    let run = match &decl.transport {
        HookTransport::Command(command) => {
            crate::plugins::run_hook_raw(
                plugin_root,
                &expand(command),
                hook_path,
                &payload,
                timeout,
            )
            .await
        }
        HookTransport::Http {
            url,
            headers,
            allowed_env_vars,
        } => {
            let headers: Vec<(String, String)> = headers
                .iter()
                .map(|(k, v)| (k.clone(), expand_env(&expand(v), allowed_env_vars)))
                .collect();
            crate::plugins::run_http_hook(&expand(url), &headers, &payload, timeout).await
        }
    };
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);

    let out = process(
        invocation.event(),
        &HookReply {
            code: run.code,
            stdout: run.stdout,
            stderr: run.stderr,
        },
    );
    if !out.ignored.is_empty() {
        tracing::info!(
            plugin,
            event = invocation.event().name(),
            fields = ?out.ignored,
            "hook set fields its event does not offer; ignored"
        );
    }
    let record = invocation.record(plugin, duration_ms, &out);
    (out, record)
}

/// Substitute `$NAME` and `${NAME}` in an HTTP header value, for the variables
/// the declaration listed in `allowedEnvVars` and no others.
///
/// A variable that is listed but unset, and one that is not listed at all, are
/// both left as written — with a warning, because an `Authorization` header
/// arriving as the literal text `Bearer $TOKEN` is otherwise indistinguishable
/// from a wrong token at the far end.
fn expand_env(value: &str, allowed: &[String]) -> String {
    expand_env_with(value, allowed, |name| std::env::var(name).ok())
}

/// The substitution itself, against any lookup — so a test can state what the
/// environment holds instead of mutating the process's own.
fn expand_env_with(
    value: &str,
    allowed: &[String],
    lookup: impl Fn(&str) -> Option<String>,
) -> String {
    if !value.contains('$') {
        return value.to_string();
    }
    let mut out = value.to_string();
    for name in allowed {
        let Some(set) = lookup(name) else {
            tracing::warn!(name, "plugin http hook allows an env var that is not set");
            continue;
        };
        out = out.replace(&format!("${{{name}}}"), &set);
        out = out.replace(&format!("${name}"), &set);
    }
    if out.contains('$') {
        tracing::warn!(
            "a plugin http hook header still holds a `$` after substitution; \
             a variable it names is missing from allowedEnvVars"
        );
    }
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::expand_env_with;

    fn allow(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    /// Both spellings the spec's own example uses, for a variable the
    /// declaration listed. Without this an `Authorization` header went out as
    /// the literal text `Bearer $TOKEN` and the endpoint answered 401.
    #[test]
    fn an_allowed_variable_is_substituted_in_either_spelling() {
        let env = |n: &str| (n == "MY_TOKEN").then(|| "s3cret".to_string());
        assert_eq!(
            expand_env_with("Bearer $MY_TOKEN", &allow(&["MY_TOKEN"]), env),
            "Bearer s3cret"
        );
        assert_eq!(
            expand_env_with("Bearer ${MY_TOKEN}", &allow(&["MY_TOKEN"]), env),
            "Bearer s3cret"
        );
    }

    /// A header is where a plugin puts a credential, so substitution is an
    /// allowlist: a variable the declaration did not name is left as written
    /// rather than read out of the runtime's environment.
    #[test]
    fn an_unlisted_variable_is_never_read() {
        let env = |_: &str| Some("s3cret".to_string());
        assert_eq!(
            expand_env_with("Bearer $OTHER_TOKEN", &allow(&["MY_TOKEN"]), env),
            "Bearer $OTHER_TOKEN"
        );
    }

    /// Listed but unset is the same as unlisted: left alone, never blanked. An
    /// empty credential reads as a wrong one at the far end.
    #[test]
    fn a_listed_but_unset_variable_is_left_alone() {
        assert_eq!(
            expand_env_with("Bearer $MY_TOKEN", &allow(&["MY_TOKEN"]), |_| None),
            "Bearer $MY_TOKEN"
        );
    }

    /// A value with no `$` never consults the environment at all.
    #[test]
    fn a_plain_header_value_is_returned_verbatim() {
        assert_eq!(
            expand_env_with("application/json", &allow(&["MY_TOKEN"]), |_| panic!(
                "must not look anything up"
            )),
            "application/json"
        );
    }
}
