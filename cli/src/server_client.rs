//! Minimal REST client for `horsie-server`, used by the agent and session
//! commands. Wire types come from `horsie_models` — no hand-rolled JSON.

use crate::error::CliError;
use horsie_models::agents::{AgentInvokeRequest, AgentInvokeResponse, AgentView};
use horsie_models::session::{SessionDetail, SessionSummary};
use horsie_models::session_api::{ApiError, GetSessionResponse, ListSessionsResponse};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct ServerClient {
    base: String,
    http: reqwest::Client,
}

impl ServerClient {
    pub fn new(server: &str) -> Self {
        Self {
            base: server.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// One JSON round-trip. Non-2xx → the server's `ApiError` message;
    /// transport failure → a "cannot reach server" error naming the base URL.
    async fn send<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, CliError> {
        let url = format!("{}{path}", self.base);
        let mut req = self.http.request(method, &url);
        if let Some(b) = body {
            req = req.json(b);
        }
        let res = req
            .send()
            .await
            .map_err(|e| CliError::Server(format!("cannot reach server at {}: {e}", self.base)))?;
        let status = res.status();
        let bytes = res
            .bytes()
            .await
            .map_err(|e| CliError::Server(format!("read response from {url}: {e}")))?;
        if !status.is_success() {
            let message = serde_json::from_slice::<ApiError>(&bytes)
                .map(|e| e.message)
                .unwrap_or_else(|_| format!("{status} {}", String::from_utf8_lossy(&bytes)));
            return Err(CliError::Server(message));
        }
        serde_json::from_slice(&bytes)
            .map_err(|e| CliError::Server(format!("bad response from {url}: {e}")))
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentView>, CliError> {
        self.send(reqwest::Method::GET, "/api/agents", None::<&str>)
            .await
    }

    pub async fn get_agent(&self, name: &str) -> Result<AgentView, CliError> {
        self.send(
            reqwest::Method::GET,
            &format!("/api/agents/{name}"),
            None::<&str>,
        )
        .await
    }

    pub async fn invoke_agent(
        &self,
        name: &str,
        req: &AgentInvokeRequest,
    ) -> Result<AgentInvokeResponse, CliError> {
        self.send(
            reqwest::Method::POST,
            &format!("/api/agents/{name}/invoke"),
            Some(req),
        )
        .await
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, CliError> {
        let resp: ListSessionsResponse = self
            .send(reqwest::Method::GET, "/api/sessions", None::<&str>)
            .await?;
        Ok(resp.sessions)
    }

    pub async fn get_session(&self, id: &str) -> Result<SessionDetail, CliError> {
        let resp: GetSessionResponse = self
            .send(
                reqwest::Method::GET,
                &format!("/api/sessions/{id}"),
                None::<&str>,
            )
            .await?;
        Ok(resp.session)
    }
}
