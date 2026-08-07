//! MCP stdio transport: a child process speaking newline-framed JSON-RPC on its
//! stdin and stdout.
//!
//! The other half of [`McpTransport`](crate::McpTransport), so
//! [`McpClient`](crate::McpClient)'s protocol logic is shared verbatim between a
//! remote endpoint and a local process. Only the framing differs.
//!
//! **This runs where the plugin files are**, which is the runtime — never the
//! server. A plugin that declares `npx …` is declaring a process next to the
//! workspace, and running it anywhere else would be both wrong and a way for a
//! plugin to execute commands on the server host.

use crate::error::McpError;
use crate::transport::McpTransport;
use async_trait::async_trait;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{Mutex, oneshot};

/// How long one request waits for its response before the server is treated as
/// stalled. MCP is request/response, so silence is a hang rather than slowness —
/// the same reasoning the HTTP transport's read timeout rests on.
const REQUEST_TIMEOUT_SECS: u64 = 30;

/// Pending responses, keyed by JSON-RPC id.
type Waiters = Arc<std::sync::Mutex<HashMap<u64, oneshot::Sender<Result<Value, McpError>>>>>;

/// A spawned MCP server process.
///
/// Owns the child and a reader task that fans responses out to whoever is
/// waiting on their id. Dropping it kills the child: a server whose client has
/// gone has nothing left to answer.
pub struct StdioTransport {
    stdin: Mutex<ChildStdin>,
    child: Mutex<Child>,
    waiters: Waiters,
    next_id: AtomicU64,
}

impl StdioTransport {
    /// Spawn `command` with `args` and `env`, and start reading its stdout.
    ///
    /// `cwd` is where the process runs — the workspace, so a server that reads
    /// files reads the ones the agent is working on. `plugin_root` becomes
    /// `CLAUDE_PLUGIN_ROOT`, the way a plugin hook already receives it: some
    /// servers read the variable rather than the `${…}` placeholder.
    pub async fn spawn(
        command: &str,
        args: &[String],
        env: &[(String, String)],
        cwd: Option<&std::path::Path>,
        plugin_root: Option<&std::path::Path>,
    ) -> Result<Self, McpError> {
        let mut cmd = tokio::process::Command::new(command);
        if let Some(root) = plugin_root {
            cmd.env("CLAUDE_PLUGIN_ROOT", root);
        }
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            // Inherited, so a server's diagnostics land in the runtime's log
            // rather than filling a pipe nobody drains — which would deadlock it.
            .stderr(Stdio::inherit())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| McpError::Transport(format!("spawn {command}: {e}")))?;

        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdin".to_string()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| McpError::Transport("child has no stdout".to_string()))?;

        let waiters: Waiters = Arc::default();
        let reader_waiters = Arc::clone(&waiters);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else {
                    // Not JSON: a server that prints to stdout alongside the
                    // protocol. Ignored rather than fatal — the framing is
                    // line-based, so the next line may well be a response.
                    continue;
                };
                // A message with no id is a notification *from* the server.
                // horsie subscribes to nothing, so there is nobody to deliver
                // it to.
                let Some(id) = msg.get("id").and_then(Value::as_u64) else {
                    continue;
                };
                let waiter = reader_waiters.lock().ok().and_then(|mut w| w.remove(&id));
                if let Some(tx) = waiter {
                    let _ = tx.send(extract(&msg));
                }
            }
            // stdout closed: the child is gone, so every outstanding request is
            // owed an answer it will never get.
            if let Ok(mut w) = reader_waiters.lock() {
                for (_, tx) in w.drain() {
                    let _ = tx.send(Err(McpError::Transport(
                        "the MCP server exited".to_string(),
                    )));
                }
            }
        });

        Ok(Self {
            stdin: Mutex::new(stdin),
            child: Mutex::new(child),
            waiters,
            next_id: AtomicU64::new(1),
        })
    }

    /// Write one JSON-RPC message, newline-framed.
    async fn write(&self, body: &Value) -> Result<(), McpError> {
        let mut line = body.to_string();
        line.push('\n');
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| McpError::Transport(format!("write to MCP server: {e}")))?;
        stdin
            .flush()
            .await
            .map_err(|e| McpError::Transport(format!("flush to MCP server: {e}")))
    }

    /// Kill the child. Idempotent — a server that already exited is shut down.
    pub async fn shutdown(&self) {
        let _ = self.child.lock().await.kill().await;
    }
}

/// A JSON-RPC response's `result`, or its `error` as an [`McpError`].
fn extract(msg: &Value) -> Result<Value, McpError> {
    if let Some(error) = msg.get("error") {
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or("unknown error");
        return Err(McpError::Protocol(message.to_string()));
    }
    Ok(msg.get("result").cloned().unwrap_or(Value::Null))
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn request(&self, method: &str, params: Value) -> Result<Value, McpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        {
            let mut waiters = self
                .waiters
                .lock()
                .map_err(|_| McpError::Transport("waiter map poisoned".to_string()))?;
            waiters.insert(id, tx);
        }
        let body = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        if let Err(e) = self.write(&body).await {
            // Registered before the write, so a failed write must unregister —
            // otherwise the id leaks and the reader would deliver to nobody.
            if let Ok(mut w) = self.waiters.lock() {
                w.remove(&id);
            }
            return Err(e);
        }
        match tokio::time::timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS), rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(McpError::Transport(
                "the MCP server dropped the request".to_string(),
            )),
            Err(_) => {
                if let Ok(mut w) = self.waiters.lock() {
                    w.remove(&id);
                }
                Err(McpError::Transport(format!(
                    "the MCP server did not answer {method} within {REQUEST_TIMEOUT_SECS}s"
                )))
            }
        }
    }

    async fn notify(&self, method: &str, params: Value) -> Result<(), McpError> {
        self.write(&json!({ "jsonrpc": "2.0", "method": method, "params": params }))
            .await
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

    /// A server scripted in `sh`: reads a line, answers it. Cheap enough to be
    /// a real child process, which is what makes these tests worth having —
    /// the framing and the process lifetime are the whole of this module.
    fn echo_server(script: &str) -> (String, Vec<String>) {
        ("sh".to_string(), vec!["-c".to_string(), script.to_string()])
    }

    async fn spawn(script: &str) -> StdioTransport {
        let (cmd, args) = echo_server(script);
        StdioTransport::spawn(&cmd, &args, &[], None, None)
            .await
            .expect("spawn")
    }

    #[tokio::test]
    async fn a_request_gets_its_response_back() {
        // Answers every line with a result carrying the same id.
        let t = spawn(
            r#"while read -r line; do
                 id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
                 printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
               done"#,
        )
        .await;
        let result = t.request("ping", json!({})).await.unwrap();
        assert_eq!(result["ok"], true);
    }

    /// Responses are correlated by id, so they may arrive in any order — which
    /// is exactly what a server answering concurrent requests does.
    #[tokio::test]
    async fn responses_are_matched_by_id_not_by_order() {
        let t = Arc::new(
            spawn(
                r#"while read -r line; do
                     id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
                     sleep 0.0$id
                     printf '{"jsonrpc":"2.0","id":%s,"result":{"id":%s}}\n' "$id" "$id"
                   done"#,
            )
            .await,
        );
        let a = {
            let t = Arc::clone(&t);
            tokio::spawn(async move { t.request("a", json!({})).await })
        };
        let b = {
            let t = Arc::clone(&t);
            tokio::spawn(async move { t.request("b", json!({})).await })
        };
        let (ra, rb) = (a.await.unwrap().unwrap(), b.await.unwrap().unwrap());
        assert_ne!(ra["id"], rb["id"], "each caller got its own response");
    }

    /// A JSON-RPC error becomes an `McpError`, not a `result` nobody checked.
    #[tokio::test]
    async fn an_error_response_is_an_error() {
        let t = spawn(
            r#"while read -r line; do
                 id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
                 printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-1,"message":"no such tool"}}\n' "$id"
               done"#,
        )
        .await;
        match t.request("tools/call", json!({})).await {
            Err(McpError::Protocol(m)) => assert!(m.contains("no such tool"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    /// Non-protocol chatter on stdout is skipped rather than desynchronising
    /// the stream — published servers print banners.
    #[tokio::test]
    async fn noise_on_stdout_does_not_break_the_framing() {
        let t = spawn(
            r#"echo "starting up..."
               while read -r line; do
                 echo "handling a request"
                 id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
                 printf '{"jsonrpc":"2.0","id":%s,"result":{"ok":true}}\n' "$id"
               done"#,
        )
        .await;
        assert_eq!(t.request("ping", json!({})).await.unwrap()["ok"], true);
    }

    /// A server that exits owes every outstanding request an answer, rather
    /// than leaving the caller to wait out the timeout.
    #[tokio::test]
    async fn a_dead_server_fails_its_requests_promptly() {
        let t = spawn("exit 0").await;
        // A moment for the reader task to observe the closed pipe.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(t.request("ping", json!({})).await.is_err());
    }

    /// The child is a plugin-owned process and learns its own root the way a
    /// hook does — a server script that reads the variable finds it set.
    #[tokio::test]
    async fn the_child_is_told_its_plugin_root() {
        let root = std::path::Path::new("/tmp/horsie-test-plugin-root");
        let (cmd, args) = echo_server(
            r#"while read -r line; do
                 id=$(printf '%s' "$line" | sed 's/.*"id":\([0-9]*\).*/\1/')
                 printf '{"jsonrpc":"2.0","id":%s,"result":{"root":"%s"}}\n' "$id" "$CLAUDE_PLUGIN_ROOT"
               done"#,
        );
        let t = StdioTransport::spawn(&cmd, &args, &[], None, Some(root))
            .await
            .expect("spawn");
        let result = t.request("ping", json!({})).await.unwrap();
        assert_eq!(result["root"], "/tmp/horsie-test-plugin-root");
    }

    #[tokio::test]
    async fn a_command_that_does_not_exist_fails_to_spawn() {
        let Err(err) = StdioTransport::spawn("horsie-no-such-binary", &[], &[], None, None).await
        else {
            panic!("a missing binary must not spawn");
        };
        assert!(matches!(err, McpError::Transport(_)));
    }
}
