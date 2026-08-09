//! The REST client behind [`FlyApi`] — the only part of the Fly vendor that
//! talks HTTP.
//!
//! Everything here is either a pure request/response translation or a thin
//! `reqwest` call, so the interesting parts (bodies, state mapping, failure
//! classification) are tested without a network and the rest is too small to
//! hide a bug.
//!
//! **Classification is the load-bearing decision.** The vendor turns
//! [`FlyError::Rejected`] into a provisioning failure the session reports, and
//! [`FlyError::Unreachable`] into "not your fault, try later". Getting this
//! backwards either buries a real misconfiguration or fails a session over a
//! blip, so the mapping lives in one tested function.

use crate::runtime_vendor::fly::{FlyApi, FlyError, Machine, MachineSpec, MachineState};
use async_trait::async_trait;
use serde_json::{Value, json};

/// Fly's public Machines API.
pub const DEFAULT_API_BASE: &str = "https://api.machines.dev/v1";

/// The machine shape and volume size runtimes get.
///
/// Sizing is a deployment choice rather than vendor logic, which is why it
/// rides the API client instead of `MachineSpec`: the vendor decides *what* to
/// run, an operator decides how big.
#[derive(Debug, Clone)]
pub struct FlyMachineSize {
    pub cpu_kind: String,
    pub cpus: u32,
    pub memory_mb: u32,
    pub volume_size_gb: u32,
}

impl Default for FlyMachineSize {
    fn default() -> Self {
        Self {
            cpu_kind: "shared".to_string(),
            cpus: 1,
            memory_mb: 1024,
            volume_size_gb: 10,
        }
    }
}

pub struct FlyHttpApi {
    client: reqwest::Client,
    base: String,
    app: String,
    token: String,
    size: FlyMachineSize,
}

impl FlyHttpApi {
    #[must_use]
    pub fn new(app: String, token: String, size: FlyMachineSize) -> Self {
        Self {
            client: reqwest::Client::new(),
            base: DEFAULT_API_BASE.to_string(),
            app,
            token,
            size,
        }
    }

    /// Point the client at a different API host — a test server, or Fly's
    /// internal endpoint from inside an organisation's network.
    #[must_use]
    pub fn with_base(mut self, base: String) -> Self {
        self.base = base.trim_end_matches('/').to_string();
        self
    }

    fn url(&self, path: &str) -> String {
        format!("{}/apps/{}{path}", self.base, self.app)
    }

    /// Send a request and return its body, already classified.
    ///
    /// Every call funnels through here so no endpoint can accidentally invent
    /// its own idea of which failures are retryable.
    async fn send(&self, req: reqwest::RequestBuilder) -> Result<Value, FlyError> {
        let response = req
            .bearer_auth(&self.token)
            .send()
            .await
            // A transport error is never an answer: DNS, TLS and connect
            // failures say nothing about the request's validity.
            .map_err(|e| FlyError::Unreachable(e.to_string()))?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(classify(status.as_u16(), &body));
        }
        // Some calls (start, stop) answer with an empty body on success.
        if body.trim().is_empty() {
            return Ok(Value::Null);
        }
        serde_json::from_str(&body)
            .map_err(|e| FlyError::Rejected(format!("unreadable fly response: {e}")))
    }
}

/// Which failures a caller may retry.
///
/// 429 and 5xx are the API's problem and pass; everything else in 4xx is the
/// request's own problem and is terminal for this attempt.
#[must_use]
pub fn classify(status: u16, body: &str) -> FlyError {
    let detail = detail_of(body).unwrap_or_else(|| body.trim().to_string());
    let message = format!("{status}: {detail}");
    if status == 429 || status >= 500 {
        FlyError::Unreachable(message)
    } else {
        FlyError::Rejected(message)
    }
}

/// Fly reports errors as `{"error": "..."}`. Fall back to the raw body, which
/// is what an HTML error page from a proxy in front of the API looks like.
fn detail_of(body: &str) -> Option<String> {
    let value: Value = serde_json::from_str(body).ok()?;
    value
        .get("error")
        .and_then(Value::as_str)
        .map(str::to_owned)
}

#[must_use]
pub fn volume_body(name: &str, region: &str, size_gb: u32) -> Value {
    json!({ "name": name, "region": region, "size_gb": size_gb })
}

#[must_use]
pub fn machine_body(spec: &MachineSpec, size: &FlyMachineSize) -> Value {
    let env: serde_json::Map<String, Value> = spec
        .env
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.clone())))
        .collect();
    let mounts: Vec<Value> = spec
        .mount
        .iter()
        .map(|(volume, path)| json!({ "volume": volume, "path": path }))
        .collect();
    json!({
        "name": spec.name,
        "region": spec.region,
        "config": {
            "image": spec.image,
            "init": { "exec": spec.command },
            "env": env,
            "mounts": mounts,
            "guest": {
                "cpu_kind": size.cpu_kind,
                "cpus": size.cpus,
                "memory_mb": size.memory_mb,
            },
            // A runtime that exits has finished; restarting it would produce a
            // machine that boots forever against a server no longer expecting
            // it. Stopping is also what makes `get` able to tell a dead runtime
            // from a live one.
            "restart": { "policy": "no" },
            "auto_destroy": false,
        }
    })
}

#[must_use]
pub fn parse_state(state: &str) -> MachineState {
    match state {
        "started" => MachineState::Started,
        "stopped" => MachineState::Stopped,
        "suspended" => MachineState::Suspended,
        _ => MachineState::Other,
    }
}

/// Pick a machine out of a list response by name.
///
/// A machine with no id is unusable, so it is treated as absent rather than
/// surfaced as a broken [`Machine`].
#[must_use]
pub fn machine_named(list: &Value, name: &str) -> Option<Machine> {
    list.as_array()?.iter().find_map(|m| {
        if m.get("name").and_then(Value::as_str) != Some(name) {
            return None;
        }
        Some(Machine {
            id: m.get("id").and_then(Value::as_str)?.to_string(),
            state: parse_state(m.get("state").and_then(Value::as_str).unwrap_or_default()),
        })
    })
}

/// Every machine in a list response, paired with its name.
///
/// An entry with no id or no name is skipped rather than surfaced: the only
/// caller is the orphan sweep, and a machine it cannot name is one it must not
/// reason about — least of all destroy.
#[must_use]
pub fn all_machines(list: &Value) -> Vec<(String, Machine)> {
    let Some(items) = list.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|m| {
            let name = m.get("name").and_then(Value::as_str)?;
            let id = m.get("id").and_then(Value::as_str)?;
            Some((
                name.to_string(),
                Machine {
                    id: id.to_string(),
                    state: parse_state(m.get("state").and_then(Value::as_str).unwrap_or_default()),
                },
            ))
        })
        .collect()
}

/// Every volume in a list response, as `(id, name)`.
///
/// Skips an entry missing either, for the same reason [`all_machines`] does:
/// its only caller deletes things, and a volume it cannot name is one it must
/// not reason about.
#[must_use]
pub fn all_volumes(list: &Value) -> Vec<(String, String)> {
    let Some(items) = list.as_array() else {
        return Vec::new();
    };
    items
        .iter()
        .filter_map(|v| {
            let id = v.get("id").and_then(Value::as_str)?;
            let name = v.get("name").and_then(Value::as_str)?;
            Some((id.to_string(), name.to_string()))
        })
        .collect()
}

fn id_of(value: &Value) -> Result<String, FlyError> {
    value
        .get("id")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| FlyError::Rejected("fly answered without an id".to_string()))
}

#[async_trait]
impl FlyApi for FlyHttpApi {
    async fn create_volume(&self, name: &str, region: &str) -> Result<String, FlyError> {
        let body = self
            .send(self.client.post(self.url("/volumes")).json(&volume_body(
                name,
                region,
                self.size.volume_size_gb,
            )))
            .await?;
        id_of(&body)
    }

    async fn create_machine(&self, spec: &MachineSpec) -> Result<String, FlyError> {
        let body = self
            .send(
                self.client
                    .post(self.url("/machines"))
                    .json(&machine_body(spec, &self.size)),
            )
            .await?;
        id_of(&body)
    }

    async fn machine_by_name(&self, name: &str) -> Result<Option<Machine>, FlyError> {
        let body = self.send(self.client.get(self.url("/machines"))).await?;
        Ok(machine_named(&body, name))
    }

    async fn machines(&self) -> Result<Vec<(String, Machine)>, FlyError> {
        let body = self.send(self.client.get(self.url("/machines"))).await?;
        Ok(all_machines(&body))
    }

    async fn start(&self, machine_id: &str) -> Result<(), FlyError> {
        self.send(
            self.client
                .post(self.url(&format!("/machines/{machine_id}/start"))),
        )
        .await
        .map(|_| ())
    }

    async fn stop(&self, machine_id: &str) -> Result<(), FlyError> {
        self.send(
            self.client
                .post(self.url(&format!("/machines/{machine_id}/stop"))),
        )
        .await
        .map(|_| ())
    }

    async fn destroy(&self, machine_id: &str) -> Result<(), FlyError> {
        let result = self
            .send(
                self.client
                    .delete(self.url(&format!("/machines/{machine_id}?force=true"))),
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            // Delete is idempotent by contract: a machine that is already gone
            // is the state the caller asked for.
            Err(FlyError::Rejected(m)) if m.starts_with("404") => Ok(()),
            Err(e) => Err(e),
        }
    }

    async fn volumes(&self) -> Result<Vec<(String, String)>, FlyError> {
        let body = self.send(self.client.get(self.url("/volumes"))).await?;
        Ok(all_volumes(&body))
    }

    async fn delete_volume(&self, volume_id: &str) -> Result<(), FlyError> {
        let result = self
            .send(
                self.client
                    .delete(self.url(&format!("/volumes/{volume_id}"))),
            )
            .await;
        match result {
            Ok(_) => Ok(()),
            Err(FlyError::Rejected(m)) if m.starts_with("404") => Ok(()),
            Err(e) => Err(e),
        }
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

    fn spec() -> MachineSpec {
        MachineSpec {
            name: "horsie-rt1".to_string(),
            image: "ghcr.io/x/runtime:1".to_string(),
            region: "iad".to_string(),
            command: vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "exec rt".to_string(),
            ],
            env: vec![("HORSIE_CONNECT_TOKEN".to_string(), "u1.rt1.tag".to_string())],
            mount: Some(("vol_1".to_string(), "/workspaces".to_string())),
        }
    }

    #[test]
    fn a_machine_body_carries_the_command_env_and_mount() {
        let body = machine_body(&spec(), &FlyMachineSize::default());
        assert_eq!(body["name"], "horsie-rt1");
        assert_eq!(body["config"]["init"]["exec"][2], "exec rt");
        assert_eq!(body["config"]["env"]["HORSIE_CONNECT_TOKEN"], "u1.rt1.tag");
        assert_eq!(body["config"]["mounts"][0]["volume"], "vol_1");
        assert_eq!(body["config"]["mounts"][0]["path"], "/workspaces");
    }

    #[test]
    fn a_machine_never_restarts_itself() {
        // A restarting machine would boot forever against a server that has
        // stopped expecting it, and would make a dead runtime indistinguishable
        // from a live one.
        let body = machine_body(&spec(), &FlyMachineSize::default());
        assert_eq!(body["config"]["restart"]["policy"], "no");
        assert_eq!(body["config"]["auto_destroy"], false);
    }

    #[test]
    fn an_unmounted_machine_sends_an_empty_mount_list() {
        // Not `null`: Fly rejects a null where it expects a list.
        let body = machine_body(
            &MachineSpec {
                mount: None,
                ..spec()
            },
            &FlyMachineSize::default(),
        );
        assert_eq!(body["config"]["mounts"], json!([]));
    }

    #[test]
    fn volumes_are_read_as_id_and_name_and_a_nameless_one_is_skipped() {
        let list = serde_json::json!([
            {"id": "vol_1", "name": "horsie_s1"},
            {"id": "vol_2"},
            {"name": "horsie_s3"},
        ]);
        assert_eq!(
            all_volumes(&list),
            vec![("vol_1".to_string(), "horsie_s1".to_string())],
            "a volume the sweep cannot name is one it must not delete"
        );
    }

    #[test]
    fn a_volume_body_carries_the_configured_size() {
        let size = FlyMachineSize {
            volume_size_gb: 25,
            ..FlyMachineSize::default()
        };
        assert_eq!(
            volume_body("horsie_rt1", "iad", size.volume_size_gb),
            json!({ "name": "horsie_rt1", "region": "iad", "size_gb": 25 })
        );
    }

    #[test]
    fn a_rate_limit_or_server_error_is_retryable() {
        assert!(matches!(classify(429, ""), FlyError::Unreachable(_)));
        assert!(matches!(classify(500, ""), FlyError::Unreachable(_)));
        assert!(matches!(classify(502, ""), FlyError::Unreachable(_)));
    }

    #[test]
    fn a_client_error_is_terminal() {
        assert!(matches!(classify(401, ""), FlyError::Rejected(_)));
        assert!(matches!(classify(404, ""), FlyError::Rejected(_)));
        assert!(matches!(classify(422, ""), FlyError::Rejected(_)));
    }

    #[test]
    fn a_classified_error_keeps_flys_own_message() {
        let e = classify(422, r#"{"error":"volume name is invalid"}"#);
        assert!(e.to_string().contains("volume name is invalid"), "{e}");
        assert!(e.to_string().contains("422"), "{e}");
    }

    #[test]
    fn a_non_json_error_body_survives_classification() {
        // A proxy in front of the API answers HTML, and losing it would leave
        // an operator with a bare status code.
        let e = classify(502, "<html>bad gateway</html>");
        assert!(e.to_string().contains("bad gateway"), "{e}");
    }

    #[test]
    fn a_machine_is_found_by_name_with_its_state() {
        let list = json!([
            { "id": "m1", "name": "horsie-other", "state": "started" },
            { "id": "m2", "name": "horsie-rt1", "state": "suspended" },
        ]);
        assert_eq!(
            machine_named(&list, "horsie-rt1"),
            Some(Machine {
                id: "m2".to_string(),
                state: MachineState::Suspended
            })
        );
        assert_eq!(machine_named(&list, "horsie-missing"), None);
    }

    #[test]
    fn an_unknown_state_is_never_read_as_gone() {
        // Guessing a machine away would destroy a workspace; a transitional
        // state is only "not usable yet".
        assert_eq!(parse_state("replacing"), MachineState::Other);
        assert_eq!(parse_state(""), MachineState::Other);
        assert_eq!(parse_state("started"), MachineState::Started);
        assert_eq!(parse_state("stopped"), MachineState::Stopped);
    }

    #[test]
    fn a_machine_without_an_id_is_treated_as_absent() {
        let list = json!([{ "name": "horsie-rt1", "state": "started" }]);
        assert_eq!(machine_named(&list, "horsie-rt1"), None);
    }

    #[test]
    fn urls_are_scoped_to_the_app() {
        let api = FlyHttpApi::new(
            "myapp".to_string(),
            "t".to_string(),
            FlyMachineSize::default(),
        )
        .with_base("https://example.test/v1/".to_string());
        assert_eq!(
            api.url("/machines"),
            "https://example.test/v1/apps/myapp/machines"
        );
    }
}
