use crate::error::ExecutorError;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio_tungstenite::{WebSocketStream, accept_async};

/// Listens for incoming WebSocket connections from runtime binaries.
pub struct RuntimeListenerServer {
    listener: TcpListener,
    local_addr: SocketAddr,
}

impl RuntimeListenerServer {
    pub async fn bind(addr: &str) -> Result<Self, ExecutorError> {
        let listener = TcpListener::bind(addr)
            .await
            .map_err(|e| ExecutorError::BindFailed(e.to_string()))?;
        let local_addr = listener
            .local_addr()
            .map_err(|e| ExecutorError::BindFailed(e.to_string()))?;
        Ok(Self {
            listener,
            local_addr,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    pub async fn accept(&self) -> Result<WebSocketStream<TcpStream>, ExecutorError> {
        let (stream, _) = self
            .listener
            .accept()
            .await
            .map_err(|e| ExecutorError::Connection(e.to_string()))?;
        accept_async(stream)
            .await
            .map_err(|e| ExecutorError::Connection(e.to_string()))
    }
}
