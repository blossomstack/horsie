/// Minimal environment allowlist for a sandboxed runtime child.
///
/// The orchestrator holds secrets (notably `ANTHROPIC_API_KEY`) that the child
/// MUST NOT inherit: a sandboxed `bash` could otherwise echo them back through
/// tool stdout into the next LLM turn. nono's network block does not close that
/// channel, so the spawn does `env_clear()` + this allowlist.
pub const SANDBOX_ENV_ALLOWLIST: &[&str] = &[
    "PATH", "HOME", "TMPDIR", "LANG", "LC_ALL", "LC_CTYPE", "TERM",
];

/// Resolve the allowlisted env vars present in the current process.
pub fn scrubbed_env() -> Vec<(String, String)> {
    SANDBOX_ENV_ALLOWLIST
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| ((*k).to_string(), v)))
        .collect()
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

    #[test]
    fn allowlist_excludes_secrets() {
        assert!(!SANDBOX_ENV_ALLOWLIST.contains(&"ANTHROPIC_API_KEY"));
        assert!(
            !SANDBOX_ENV_ALLOWLIST
                .iter()
                .any(|k| k.contains("KEY") || k.contains("TOKEN") || k.contains("SECRET"))
        );
    }

    #[test]
    fn scrubbed_env_only_returns_allowlisted_keys() {
        for (k, _) in scrubbed_env() {
            assert!(SANDBOX_ENV_ALLOWLIST.contains(&k.as_str()), "leaked {k}");
        }
    }

    /// The spawn recipe (`env_clear()` + `scrubbed_env()`) must wipe a secret that
    /// would otherwise be inherited — verified by a real child process. We seed the
    /// secret via `.env(...)` *before* `env_clear()` (simulating inheritance) and
    /// confirm the child sees it empty.
    #[tokio::test]
    async fn spawned_child_does_not_see_secret() {
        if which_bash().is_none() {
            eprintln!("skipping: no bash on PATH");
            return;
        }
        let mut cmd = tokio::process::Command::new("bash");
        cmd.arg("-c").arg("printf '%s' \"$ANTHROPIC_API_KEY\"");
        cmd.env("ANTHROPIC_API_KEY", "leak-me");
        cmd.env_clear();
        for (k, v) in scrubbed_env() {
            cmd.env(k, v);
        }
        let out = cmd.output().await.unwrap();
        assert!(out.status.success());
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "",
            "ANTHROPIC_API_KEY leaked into the scrubbed child env"
        );
    }

    fn which_bash() -> Option<std::path::PathBuf> {
        std::env::var_os("PATH").and_then(|paths| {
            std::env::split_paths(&paths)
                .map(|p| p.join("bash"))
                .find(|p| p.exists())
        })
    }
}
