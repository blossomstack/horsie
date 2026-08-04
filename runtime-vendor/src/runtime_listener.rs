use crate::error::ExecutorError;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};

/// Where the executor listens for runtime children to connect.
#[derive(Debug, Clone)]
pub enum RuntimeEndpoint {
    /// TCP/WebSocket (server / distributed mode). Child is spawned with `ws://<addr>`.
    Tcp(SocketAddr),
    /// Unix domain socket (CLI / single-process mode). Child gets `unix:<path>`.
    Unix(PathBuf),
}

enum Listener {
    Tcp(TcpListener),
    Unix(UnixListener),
}

/// One accepted socket, statically typed by socket family so the generic
/// connection handler can be monomorphized per socket type.
///
/// Handed over before the WebSocket handshake deliberately: the handshake talks
/// to the peer, so it fails for peer-shaped reasons (a runtime child killed
/// between `connect()` and its first byte). Doing it on the accept path would
/// make one such peer indistinguishable from the listener itself failing.
pub enum AcceptedStream {
    Tcp(TcpStream),
    Unix(UnixStream),
}

/// Listens for incoming WebSocket connections from runtime binaries over either
/// TCP or a unix socket, per the configured [`RuntimeEndpoint`].
pub struct RuntimeListenerServer {
    listener: Listener,
    endpoint: RuntimeEndpoint,
}

impl RuntimeListenerServer {
    pub async fn bind(endpoint: RuntimeEndpoint) -> Result<Self, ExecutorError> {
        let listener = match &endpoint {
            RuntimeEndpoint::Tcp(addr) => Listener::Tcp(
                TcpListener::bind(addr)
                    .await
                    .map_err(|e| ExecutorError::BindFailed(e.to_string()))?,
            ),
            RuntimeEndpoint::Unix(path) => {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)
                        .map_err(|e| ExecutorError::BindFailed(e.to_string()))?;
                    #[cfg(unix)]
                    {
                        use std::os::unix::fs::PermissionsExt;
                        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
                            .map_err(|e| ExecutorError::BindFailed(e.to_string()))?;
                    }
                }
                // Unlink any stale socket so bind does not fail with EADDRINUSE.
                let _ = std::fs::remove_file(path);
                Listener::Unix(
                    UnixListener::bind(path)
                        .map_err(|e| ExecutorError::BindFailed(e.to_string()))?,
                )
            }
        };
        Ok(Self { listener, endpoint })
    }

    pub fn endpoint(&self) -> &RuntimeEndpoint {
        &self.endpoint
    }

    /// The bound TCP address when listening on TCP (server mode spawns `ws://<addr>`).
    pub fn tcp_addr(&self) -> Option<SocketAddr> {
        match &self.listener {
            Listener::Tcp(l) => l.local_addr().ok(),
            Listener::Unix(_) => None,
        }
    }

    /// Accept the next connection. An error here is the listener's own — a peer
    /// that connects and hangs up is a successful accept whose handshake fails
    /// later, on its own task.
    pub async fn accept(&self) -> Result<AcceptedStream, ExecutorError> {
        match &self.listener {
            Listener::Tcp(l) => {
                let (stream, _) = l
                    .accept()
                    .await
                    .map_err(|e| ExecutorError::Connection(e.to_string()))?;
                Ok(AcceptedStream::Tcp(stream))
            }
            Listener::Unix(l) => {
                let (stream, _) = l
                    .accept()
                    .await
                    .map_err(|e| ExecutorError::Connection(e.to_string()))?;
                Ok(AcceptedStream::Unix(stream))
            }
        }
    }
}

impl Drop for RuntimeListenerServer {
    fn drop(&mut self) {
        // Unlink the unix socket on shutdown; missing-file is fine. Safe to do
        // unconditionally only because the path is this process's own (see
        // `runtime_socket_path` in the CLI): a shared path would mean unlinking
        // a file another live agent had just bound, leaving it unreachable.
        if let RuntimeEndpoint::Unix(path) = &self.endpoint {
            let _ = std::fs::remove_file(path);
        }
    }
}
