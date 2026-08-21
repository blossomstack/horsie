//! Minimal REST client for `horsie-server`, used by the agent and session
//! commands. Wire types come from `horsie_models` — no hand-rolled JSON.

use crate::error::CliError;
use horsie_models::agents::{AgentInvokeRequest, AgentInvokeResponse, AgentView};
use horsie_models::projects::ProjectView;
use horsie_models::routines::{RoutineRunResponse, RoutineView};
use horsie_models::session::{SessionDetail, SessionSummary};
use horsie_models::session_api::{
    AgentDocument, ApiError, GetAgentResponse, GetSessionResponse, ListSessionsResponse,
};
use horsie_models::workflow::{
    WorkflowInput, WorkflowRetryRequest, WorkflowRunGraph, WorkflowRunRequest, WorkflowRunResponse,
    WorkflowView,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

pub struct ServerClient {
    base: String,
    /// The project every path below is relative to. Resolved once, at
    /// construction, because it takes a round trip and every call needs it.
    project: String,
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
    pub async fn new(server: &str, project: Option<&str>) -> Result<Self, CliError> {
        let base = server.trim_end_matches('/').to_string();
        let http = reqwest::Client::new();
        let token = crate::auth::resolve_token(server).await?;
        let project = resolve_project(&http, &base, token.as_deref(), project).await?;
        Ok(Self {
            base,
            project,
            http,
            token,
        })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn project(&self) -> &str {
        &self.project
    }

    /// Every scoped route lives under this. `path` is relative to the project,
    /// which is why none of the callers below writes `/api`.
    fn url(&self, path: &str) -> String {
        format!("{}/api/p/{}{path}", self.base, self.project)
    }

    /// One JSON round-trip. Non-2xx → the server's `ApiError` message;
    /// transport failure → a "cannot reach server" error naming the base URL.
    async fn send<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<T, CliError> {
        let url = self.url(path);
        let bytes = self.request_bytes(method, path, body).await?;
        serde_json::from_slice(&bytes)
            .map_err(|e| CliError::Server(format!("bad response from {url}: {e}")))
    }

    /// As [`send`](Self::send), for the endpoints that answer `202`/`204` with no
    /// body — a retry, a delete. Deserializing `()` from an empty body fails, so
    /// these need the same error handling without the last step.
    async fn send_no_body<B: Serialize + ?Sized>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<(), CliError> {
        self.request_bytes(method, path, body).await.map(|_| ())
    }

    async fn request_bytes<B: Serialize + ?Sized>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: Option<&B>,
    ) -> Result<Vec<u8>, CliError> {
        let url = self.url(path);
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
        Ok(bytes.to_vec())
    }

    pub async fn list_agents(&self) -> Result<Vec<AgentView>, CliError> {
        self.send(reqwest::Method::GET, "/agents", None::<&str>)
            .await
    }

    pub async fn get_agent(&self, name: &str) -> Result<AgentView, CliError> {
        self.send(
            reqwest::Method::GET,
            &format!("/agents/{name}"),
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
            &format!("/agents/{name}/invoke"),
            Some(req),
        )
        .await
    }

    pub async fn list_workflows(&self) -> Result<Vec<WorkflowView>, CliError> {
        self.send(reqwest::Method::GET, "/workflows", None::<&str>)
            .await
    }

    pub async fn get_workflow(&self, name: &str) -> Result<WorkflowView, CliError> {
        self.send(
            reqwest::Method::GET,
            &format!("/workflows/{name}"),
            None::<&str>,
        )
        .await
    }

    pub async fn run_workflow(
        &self,
        name: &str,
        req: &WorkflowRunRequest,
    ) -> Result<WorkflowRunResponse, CliError> {
        self.send(
            reqwest::Method::POST,
            &format!("/workflows/{name}/runs"),
            Some(req),
        )
        .await
    }

    pub async fn create_workflow(&self, input: &WorkflowInput) -> Result<WorkflowView, CliError> {
        self.send(reqwest::Method::POST, "/workflows", Some(input))
            .await
    }

    /// Full replace; the path name is the id of record, so the body's name has
    /// to match it.
    pub async fn replace_workflow(
        &self,
        name: &str,
        input: &WorkflowInput,
    ) -> Result<WorkflowView, CliError> {
        self.send(
            reqwest::Method::PUT,
            &format!("/workflows/{name}"),
            Some(input),
        )
        .await
    }

    pub async fn delete_workflow(&self, name: &str) -> Result<(), CliError> {
        self.send_no_body(
            reqwest::Method::DELETE,
            &format!("/workflows/{name}"),
            None::<&str>,
        )
        .await
    }

    /// A run, projected onto its graph. Keyed by session id: a run is a
    /// session.
    pub async fn workflow_run(&self, session_id: &str) -> Result<WorkflowRunGraph, CliError> {
        self.send(
            reqwest::Method::GET,
            &format!("/sessions/{session_id}/workflow"),
            None::<&str>,
        )
        .await
    }

    /// Re-run one step execution. Keyed by session id, like the graph read.
    pub async fn retry_workflow_step(
        &self,
        session_id: &str,
        step_index: u32,
    ) -> Result<(), CliError> {
        self.send_no_body(
            reqwest::Method::POST,
            &format!("/sessions/{session_id}/workflow/retry"),
            Some(&WorkflowRetryRequest { step_index }),
        )
        .await
    }

    pub async fn list_sessions(&self) -> Result<Vec<SessionSummary>, CliError> {
        let resp: ListSessionsResponse = self
            .send(reqwest::Method::GET, "/sessions", None::<&str>)
            .await?;
        Ok(resp.sessions)
    }

    pub async fn get_session(&self, id: &str) -> Result<SessionDetail, CliError> {
        let resp: GetSessionResponse = self
            .send(
                reqwest::Method::GET,
                &format!("/sessions/{id}"),
                None::<&str>,
            )
            .await?;
        Ok(resp.session)
    }

    pub async fn get_agent_document(
        &self,
        id: &str,
        agent_id: &str,
    ) -> Result<AgentDocument, CliError> {
        let resp: GetAgentResponse = self
            .send(
                reqwest::Method::GET,
                &format!("/sessions/{id}/agents/{agent_id}"),
                None::<&str>,
            )
            .await?;
        Ok(resp.agent)
    }

    pub async fn list_routines(&self) -> Result<Vec<RoutineView>, CliError> {
        self.send(reqwest::Method::GET, "/routines", None::<&str>)
            .await
    }

    pub async fn get_routine(&self, name: &str) -> Result<RoutineView, CliError> {
        self.send(
            reqwest::Method::GET,
            &format!("/routines/{name}"),
            None::<&str>,
        )
        .await
    }

    pub async fn run_routine(&self, name: &str) -> Result<RoutineRunResponse, CliError> {
        self.send(
            reqwest::Method::POST,
            &format!("/routines/{name}/run"),
            None::<&str>,
        )
        .await
    }
}

/// Which project this invocation acts in.
///
/// `--project` accepts an **id or a name**, because an id is what a URL carries
/// and a name is what a person remembers, and the list that settles it has to
/// be fetched either way. An id wins on a tie: it is the unambiguous form, and
/// a project named after another's id is a coincidence rather than a request.
///
/// Absent → the account's default project, read from the server rather than
/// remembered locally. A stored id would be wrong the moment the user pointed
/// `--server` somewhere else, and would fail as a 404 on a route that has
/// nothing to do with projects.
/// [`resolve_project`] for a caller that has no [`ServerClient`] — `horsie
/// connect`, which needs the project before it has anything to talk to.
pub async fn project_for(
    server: &str,
    token: Option<&str>,
    wanted: Option<&str>,
) -> Result<String, CliError> {
    resolve_project(
        &reqwest::Client::new(),
        server.trim_end_matches('/'),
        token,
        wanted,
    )
    .await
}

async fn resolve_project(
    http: &reqwest::Client,
    base: &str,
    token: Option<&str>,
    wanted: Option<&str>,
) -> Result<String, CliError> {
    let mut req = http
        .get(format!("{base}/api/projects"))
        // A startup step with a person waiting on it. Without a deadline a
        // server that accepts the connection and never answers hangs the
        // command for ever, which is how `horsie connect` first failed against
        // a stand-in that spoke only the vendor socket.
        .timeout(std::time::Duration::from_secs(10));
    if let Some(t) = token {
        req = req.bearer_auth(t);
    }
    let res = req
        .send()
        .await
        .map_err(|e| CliError::Server(format!("cannot reach server at {base}: {e}")))?;
    if res.status() == reqwest::StatusCode::UNAUTHORIZED {
        return Err(CliError::Server(format!(
            "not authorized for {base} — run `horsie auth login --server {base}`"
        )));
    }
    let bytes = res
        .bytes()
        .await
        .map_err(|e| CliError::Server(format!("read the project list from {base}: {e}")))?;
    let projects: Vec<ProjectView> = serde_json::from_slice(&bytes)
        .map_err(|e| CliError::Server(format!("bad project list from {base}: {e}")))?;

    match wanted {
        Some(w) => projects
            .iter()
            .find(|p| p.id == w)
            .or_else(|| projects.iter().find(|p| p.name == w))
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                let known: Vec<&str> = projects.iter().map(|p| p.name.as_str()).collect();
                CliError::Server(format!(
                    "no project '{w}' on {base}; this account has: {}",
                    known.join(", ")
                ))
            }),
        None => projects
            .iter()
            .find(|p| p.is_default)
            .or_else(|| projects.first())
            .map(|p| p.id.clone())
            .ok_or_else(|| {
                CliError::Server(format!("{base} reports no projects for this account"))
            }),
    }
}
