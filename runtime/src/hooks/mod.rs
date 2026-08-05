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
    HookDecl, HookEvent, HookInvocation, HookOutput, HookReply, matcher_selects, process,
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
    let command = decl
        .command
        .replace("${CLAUDE_PLUGIN_ROOT}", &plugin_root.to_string_lossy());
    let timeout = decl.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs);

    let started = Instant::now();
    let run = crate::plugins::run_hook_raw(
        plugin_root,
        &command,
        hook_path,
        &invocation.payload(),
        timeout,
    )
    .await;
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
