//! Minimal REST client for `horsie-server`, used by the agent and session
//! commands. Wire types come from `horsie_models` — no hand-rolled JSON.

use crate::error::CliError;
use horsie_models::agents::{AgentInvokeRequest, AgentInvokeResponse, AgentView};
use horsie_models::routines::{RoutineRunResponse, RoutineView};
use horsie_models::session::{SessionDetail, SessionSummary};
use horsie_models::session_api::{ApiError, GetSessionResponse, ListSessionsResponse};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct ServerClient {
    base: String,
    http: reqwest::Client,
    /// Bearer sent with every request. `None` when the user has no credential
    /// for this server — which is correct against a server running with
    /// authentication disabled, and produces a 401 with a "run `horsie auth
    /// login`" message against one that does not.
    token: Option<String>,
}

impl ServerClient {
    /// Build a client, picking up any stored credential for `server`. Async
    /// because resolving a credential may refresh an expired access token.
    /// There is deliberately no un-authenticated constructor: a call site that
    /// forgets one would fail only against servers with auth on, which is the
    /// configuration least likely to be exercised while developing.
    pub async fn new(server: &str) -> Result<Self, CliError> {
        Ok(Self {
            base: server.trim_end_matches('/').to_string(),
            http: reqwest::Client::new(),
            token: crate::auth::resolve_token(server).await?,
        })
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
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
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
        if status == reqwest::StatusCode::UNAUTHORIZED {
            // Nothing about this request can be fixed by retrying it, so say
            // what to do instead of relaying "unauthorized".
            return Err(CliError::Server(format!(
                "not authorized for {base} — run `horsie auth login --server {base}`",
                base = self.base
            )));
        }
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

    pub async fn list_routines(&self) -> Result<Vec<RoutineView>, CliError> {
        self.send(reqwest::Method::GET, "/api/routines", None::<&str>)
            .await
    }

    pub async fn get_routine(&self, name: &str) -> Result<RoutineView, CliError> {
        self.send(
            reqwest::Method::GET,
            &format!("/api/routines/{name}"),
            None::<&str>,
        )
        .await
    }

    pub async fn run_routine(&self, name: &str) -> Result<RoutineRunResponse, CliError> {
        self.send(
            reqwest::Method::POST,
            &format!("/api/routines/{name}/run"),
            None::<&str>,
        )
        .await
    }
}
