//! The command line a container or micro-VM runs to become a runtime.
//!
//! Shared by every vendor that hands a substrate an image and an argv — Fly
//! machines and velos containers build the identical line, because what the
//! runtime needs is the same wherever it runs: its workspace directories, and
//! then `exec` so it becomes PID 1.
//!
//! Being PID 1 is load-bearing rather than tidy. The runtime's exit *is* the
//! container's exit, so a substrate that reports a live container is reporting
//! a live runtime — which is what lets a vendor tell "still booting" from
//! "died" without asking the runtime anything.

/// Make the workspace directories, then `exec` the runtime.
///
/// No `--sandbox-caps`: the container is already the isolation boundary, and
/// applying a second one inside it only breaks the runtime's own writes.
#[must_use]
pub fn build_runtime_command(
    runtime_bin: &str,
    endpoint: &str,
    runtime_id: &str,
    workspaces: &[(String, String)],
) -> Vec<String> {
    let mut exec_line = format!(
        "exec {} --endpoint {} --runtime-id {}",
        shell_quote(runtime_bin),
        shell_quote(endpoint),
        shell_quote(runtime_id),
    );
    for (name, path) in workspaces {
        exec_line.push_str(&format!(
            " --workspace {}",
            shell_quote(&format!("{name}={path}"))
        ));
    }
    let script = if workspaces.is_empty() {
        exec_line
    } else {
        let dirs = workspaces
            .iter()
            .map(|(_, path)| shell_quote(path))
            .collect::<Vec<_>>()
            .join(" ");
        format!("mkdir -p {dirs} && {exec_line}")
    };
    vec!["/bin/sh".to_string(), "-c".to_string(), script]
}

/// POSIX single-quote a value so it survives `sh -c` verbatim. Workspace names
/// come from session config, so quote defensively.
#[must_use]
pub fn shell_quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('\'');
    for c in s.chars() {
        if c == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(c);
        }
    }
    out.push('\'');
    out
}

/// Where a named workspace lands under a vendor's workspace root.
///
/// Rejects anything that could escape the root. A workspace name reaches here
/// from session config, and `..` in one would put an agent's writes wherever it
/// liked inside the container.
#[must_use]
pub fn workspace_paths(root: &str, names: &[String]) -> Vec<(String, String)> {
    let root = root.trim_end_matches('/');
    names
        .iter()
        .filter(|n| !n.is_empty() && !n.contains('/') && !n.contains(".."))
        .map(|n| (n.clone(), format!("{root}/{n}")))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_runtime_becomes_pid_one_after_its_directories_exist() {
        let cmd = build_runtime_command(
            "horsie-runtime",
            "ws://server/api/runtime/connect",
            "rt-1",
            &[("main".to_string(), "/workspaces/main".to_string())],
        );
        assert_eq!(cmd[0], "/bin/sh");
        assert_eq!(cmd[1], "-c");
        assert!(
            cmd[2].starts_with("mkdir -p '/workspaces/main' && exec "),
            "{}",
            cmd[2]
        );
        assert!(cmd[2].contains("--runtime-id 'rt-1'"), "{}", cmd[2]);
    }

    #[test]
    fn a_runtime_with_no_workspaces_skips_the_mkdir() {
        let cmd = build_runtime_command("horsie-runtime", "ws://s/", "rt-1", &[]);
        assert!(cmd[2].starts_with("exec "), "{}", cmd[2]);
    }

    #[test]
    fn a_quote_in_a_value_cannot_end_the_quoting() {
        // The whole point of quoting: a workspace path is user-derived, and a
        // stray quote would otherwise let it append shell of its own.
        assert_eq!(shell_quote("a'b"), "'a'\\''b'");
        let cmd = build_runtime_command(
            "rt",
            "ws://s/",
            "id",
            &[("w".to_string(), "/tmp/a'; rm -rf /".to_string())],
        );
        assert!(cmd[2].contains("'/tmp/a'\\''; rm -rf /'"), "{}", cmd[2]);
    }

    #[test]
    fn a_workspace_name_cannot_escape_its_root() {
        let paths = workspace_paths(
            "/workspaces/",
            &[
                "main".to_string(),
                "..".to_string(),
                "a/b".to_string(),
                "../etc".to_string(),
                String::new(),
            ],
        );
        assert_eq!(
            paths,
            vec![("main".to_string(), "/workspaces/main".to_string())]
        );
    }
}
