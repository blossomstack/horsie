use crate::error::ExecutorError;
use std::net::SocketAddr;
use std::path::PathBuf;
use tokio::net::{TcpListener, TcpStream, UnixListener, UnixStream};
use tokio_tungstenite::{WebSocketStream, accept_async};

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

/// One accepted runtime link, statically typed by socket family so the generic
/// connection handler can be monomorphized per socket type.
pub enum AcceptedConn {
    Tcp(WebSocketStream<TcpStream>),
    Unix(WebSocketStream<UnixStream>),
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

    pub async fn accept(&self) -> Result<AcceptedConn, ExecutorError> {
        match &self.listener {
            Listener::Tcp(l) => {
                let (stream, _) = l
                    .accept()
                    .await
                    .map_err(|e| ExecutorError::Connection(e.to_string()))?;
                let ws = accept_async(stream)
                    .await
                    .map_err(|e| ExecutorError::Connection(e.to_string()))?;
                Ok(AcceptedConn::Tcp(ws))
            }
            Listener::Unix(l) => {
                let (stream, _) = l
                    .accept()
                    .await
                    .map_err(|e| ExecutorError::Connection(e.to_string()))?;
                let ws = accept_async(stream)
                    .await
                    .map_err(|e| ExecutorError::Connection(e.to_string()))?;
                Ok(AcceptedConn::Unix(ws))
            }
        }
    }
}

impl Drop for RuntimeListenerServer {
    fn drop(&mut self) {
        // Unlink the unix socket on shutdown; missing-file is fine.
        if let RuntimeEndpoint::Unix(path) = &self.endpoint {
            let _ = std::fs::remove_file(path);
        }
    }
}
