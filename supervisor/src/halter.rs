//! Mint-at-spawn integration with halter, the policy-governed credential
//! proxy. When a job carries a halter run-policy and the daemon is configured
//! against a halter deployment, the job spawn mints a fresh policy-bound token
//! from halter's admin API, materializes a synthetic per-job home directory,
//! opens that home in the job's capability spec, and injects the env the
//! sandboxed runtime needs to self-provision through the proxy listener.
//!
//! The halter *server location* (`admin_url` / `proxy_url`) is genuinely
//! deployment-global, so it lives in [`HalterMinter`]. The *policy* and its TTL
//! are a per-run resource — like the workflow and capability files — carried on
//! each job as a [`HalterRunPolicy`].
//!
//! Fail closed: a job that asked for a halter policy but cannot mint must abort
//! the spawn — a job silently running without its credential proxy is the
//! illegal state.

use models::capabilities::{Access, CapabilitySpec, DirGrant, Grant};
use models::daemon::HalterRunPolicy;
use models::executor::EnvVar;
use serde::Deserialize;
use std::path::Path;

/// Env var carrying the minted policy-bound token into the runtime child.
pub const ENV_HALTER_TOKEN: &str = "HALTER_TOKEN";
/// Env var carrying the proxy-listener base URL into the runtime child.
pub const ENV_HALTER_URL: &str = "HALTER_URL";

/// Default TTL for minted halter tokens when a policy omits `params.ttlSeconds`:
/// one hour. The per-run `HalterRunPolicy`/`HalterPolicyParams` are the generated
/// `models::daemon` wire types; horsie applies this default when the optional
/// `ttlSeconds` is absent.
const DEFAULT_TTL_SECONDS: u64 = 3600;

/// An opaque, policy-bound bearer token minted by halter for exactly one job
/// spawn. Newtype so a random string can never be passed where a minted token
/// is required.
#[derive(Debug)]
pub struct JobToken(String);

/// Wire mirror of halter's admin `POST /mint` response. halter owns this
/// contract (camelCase JSON), so it is a hand-written serde struct here — not a
/// horsie fluorite protocol type. Fields we don't consume (`expiresAtMs`) are
/// ignored by serde's default unknown-field handling.
#[derive(Debug, Deserialize)]
struct MintResponse {
    token: String,
}

/// Daemon-side halter handle: the deployment-global server location plus the
/// HTTP client that mints one token per job spawn against `{admin_url}/mint`.
/// The policy and TTL are NOT held here — they are per-run, supplied to
/// [`HalterMinter::mint`] from the job's [`HalterRunPolicy`].
#[derive(Clone)]
pub struct HalterMinter {
    admin_url: String,
    proxy_url: String,
    client: reqwest::Client,
}

impl HalterMinter {
    pub fn new(admin_url: String, proxy_url: String) -> Self {
        Self {
            admin_url,
            proxy_url,
            client: reqwest::Client::new(),
        }
    }

    /// Mint one policy-bound token from the per-run policy: POST the policy doc
    /// to halter's admin API with the policy's TTL, return the opaque token.
    /// Every error path is a spawn-aborting failure (fail closed).
    pub async fn mint(&self, policy: &HalterRunPolicy) -> Result<JobToken, String> {
        let url = format!("{}/mint", self.admin_url.trim_end_matches('/'));
        // The effective TTL: the per-run `params.ttlSeconds` if present, else the
        // one-hour default. `params` and `ttlSeconds` are both optional.
        let ttl_seconds = policy
            .params
            .as_ref()
            .and_then(|p| p.ttl_seconds)
            .unwrap_or(DEFAULT_TTL_SECONDS);
        // halter's admin wire contract is camelCase: {"policy": ..., "ttlSeconds": ...}.
        let body = serde_json::json!({
            "policy": policy.policy,
            "ttlSeconds": ttl_seconds,
        });
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(|e| format!("halter mint: POST {url} failed: {e}"))?;
        let status = resp.status();
        if !status.is_success() {
            let detail = resp.text().await.unwrap_or_default();
            return Err(format!(
                "halter mint: {url} returned HTTP {status}: {}",
                detail.trim()
            ));
        }
        let minted: MintResponse = resp
            .json()
            .await
            .map_err(|e| format!("halter mint: invalid response from {url}: {e}"))?;
        Ok(JobToken(minted.token))
    }
}

/// Everything one successful mint contributes to a job spawn: the env injected
/// into the runtime child and the extra capability grant opening its home.
pub struct HalterProvision {
    /// `HALTER_TOKEN` / `HALTER_URL` / `HOME` / `GH_CONFIG_DIR` for the child.
    pub env: Vec<EnvVar>,
    /// ReadWrite grant over the job's synthetic home directory.
    pub home_grant: Grant,
}

/// Mint a token for the per-run policy and materialize the job's synthetic home
/// under `<job_dir>/home`, next to the job's capability file. Fail closed: any
/// error must abort the spawn.
pub async fn provision_for_spawn(
    minter: &HalterMinter,
    policy: &HalterRunPolicy,
    job_dir: &Path,
) -> Result<HalterProvision, String> {
    let token = minter.mint(policy).await?;
    materialize_home(job_dir, &minter.proxy_url, token)
}

/// Create the synthetic home (plus the `gh` config dir the runtime writes
/// into) and derive the env + grant for it. Pure filesystem — no HTTP — so the
/// spawn-side wiring is testable without a halter.
fn materialize_home(
    job_dir: &Path,
    proxy_url: &str,
    token: JobToken,
) -> Result<HalterProvision, String> {
    let home = job_dir.join("home");
    let gh_config_dir = home.join(".config").join("gh");
    std::fs::create_dir_all(&gh_config_dir)
        .map_err(|e| format!("halter: cannot create job home {}: {e}", home.display()))?;
    Ok(HalterProvision {
        env: vec![
            EnvVar {
                name: ENV_HALTER_TOKEN.to_string(),
                value: token.0,
            },
            EnvVar {
                name: ENV_HALTER_URL.to_string(),
                value: proxy_url.to_string(),
            },
            EnvVar {
                name: "HOME".to_string(),
                value: home.to_string_lossy().into_owned(),
            },
            EnvVar {
                name: "GH_CONFIG_DIR".to_string(),
                value: gh_config_dir.to_string_lossy().into_owned(),
            },
        ],
        home_grant: Grant::Dir(DirGrant {
            path: home.to_string_lossy().into_owned(),
            access: Access::ReadWrite,
        }),
    })
}

/// Fold an optional spawn provision into the job's launch inputs: the
/// capability spec gains the synthetic-home grant and the runtime env gains the
/// halter variables. `None` (no minter configured, or a job that carries no
/// halter policy) passes both through untouched — byte-for-byte identical to a
/// daemon without halter.
pub fn apply_provision(
    caps: CapabilitySpec,
    provision: Option<HalterProvision>,
) -> (CapabilitySpec, Vec<EnvVar>) {
    match provision {
        None => (caps, Vec::new()),
        Some(p) => {
            let mut caps = caps;
            caps.grants.push(p.home_grant);
            (caps, p.env)
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use axum::Json;
    use axum::extract::State;
    use axum::routing::post;
    use models::capabilities::{BlockNetwork, NetworkPolicy};
    use models::daemon::HalterPolicyParams;
    use serde_json::{Value, json};
    use std::sync::{Arc, Mutex};

    /// What the mock admin server should answer to `POST /mint`.
    #[derive(Clone)]
    enum MintReply {
        Token(&'static str),
        Forbidden,
    }

    /// Spin a mock halter admin API on an ephemeral port. Returns its base URL
    /// and the captured request body of the last `/mint` call.
    async fn mock_admin(reply: MintReply) -> (String, Arc<Mutex<Option<Value>>>) {
        let captured: Arc<Mutex<Option<Value>>> = Arc::new(Mutex::new(None));
        let state = (reply, captured.clone());
        let app = axum::Router::new()
            .route(
                "/mint",
                post(
                    |State((reply, captured)): State<(MintReply, Arc<Mutex<Option<Value>>>)>,
                     Json(body): Json<Value>| async move {
                        *captured.lock().unwrap() = Some(body);
                        match reply {
                            MintReply::Token(t) => (
                                axum::http::StatusCode::OK,
                                Json(json!({ "token": t, "expiresAtMs": 1_000_u64 })),
                            ),
                            MintReply::Forbidden => (
                                axum::http::StatusCode::FORBIDDEN,
                                Json(json!({ "error": "policy rejected" })),
                            ),
                        }
                    },
                ),
            )
            .with_state(state);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (format!("http://{addr}"), captured)
    }

    fn policy(ttl: u64) -> HalterRunPolicy {
        HalterRunPolicy {
            policy: json!({ "targets": [{ "name": "github" }] }),
            params: Some(HalterPolicyParams {
                ttl_seconds: Some(ttl),
            }),
        }
    }

    /// Parse a per-run halter policy file (the CLI's `--halter-policy` path) into
    /// the generated wire type, mirroring how the daemon deserializes it.
    fn parse_policy(text: &str) -> Result<HalterRunPolicy, serde_json::Error> {
        serde_json::from_str(text)
    }

    fn minter(admin_url: &str) -> HalterMinter {
        HalterMinter::new(admin_url.to_string(), "http://127.0.0.1:9090".to_string())
    }

    #[test]
    fn parse_full_doc_reads_policy_and_ttl() {
        let p = parse_policy(
            r#"{ "policy": { "targets": [{ "name": "github" }] }, "params": { "ttlSeconds": 900 } }"#,
        )
        .unwrap();
        assert_eq!(p.params.and_then(|p| p.ttl_seconds), Some(900));
        assert_eq!(p.policy["targets"][0]["name"], json!("github"));
    }

    #[test]
    fn missing_params_yields_default_ttl() {
        // `params` absent → effective TTL is the one-hour default at mint time.
        let p = parse_policy(r#"{ "policy": { "k": "v" } }"#).unwrap();
        assert!(p.params.is_none());
        let ttl = p
            .params
            .as_ref()
            .and_then(|p| p.ttl_seconds)
            .unwrap_or(DEFAULT_TTL_SECONDS);
        assert_eq!(ttl, 3600);
    }

    #[test]
    fn missing_ttl_seconds_yields_default_ttl() {
        // `params` present but `ttlSeconds` absent → still the one-hour default.
        let p = parse_policy(r#"{ "policy": {}, "params": {} }"#).unwrap();
        assert_eq!(p.params.as_ref().and_then(|p| p.ttl_seconds), None);
        let ttl = p
            .params
            .as_ref()
            .and_then(|p| p.ttl_seconds)
            .unwrap_or(DEFAULT_TTL_SECONDS);
        assert_eq!(ttl, 3600);
    }

    #[test]
    fn parse_rejects_missing_policy() {
        // The opaque policy is required — a doc with only params cannot mint.
        assert!(parse_policy(r#"{ "params": { "ttlSeconds": 60 } }"#).is_err());
    }

    #[tokio::test]
    async fn mint_defaults_ttl_when_params_absent() {
        // A policy with no `params` mints with the one-hour default TTL.
        let (url, captured) = mock_admin(MintReply::Token("tok-def")).await;
        let policy = HalterRunPolicy {
            policy: json!({ "k": "v" }),
            params: None,
        };
        let token = minter(&url).mint(&policy).await.unwrap();
        assert_eq!(token.0, "tok-def");
        let body = captured.lock().unwrap().clone().expect("body captured");
        assert_eq!(body["ttlSeconds"], json!(3600));
    }

    #[tokio::test]
    async fn mint_posts_camelcase_body_and_returns_token() {
        let (url, captured) = mock_admin(MintReply::Token("tok-abc")).await;
        let token = minter(&url).mint(&policy(900)).await.unwrap();
        assert_eq!(token.0, "tok-abc");
        let body = captured.lock().unwrap().clone().expect("body captured");
        // halter's wire contract: the policy doc passes through verbatim and the
        // TTL field is camelCase, sourced from the per-run params.
        assert_eq!(body["ttlSeconds"], json!(900));
        assert_eq!(body["policy"]["targets"][0]["name"], json!("github"));
    }

    #[tokio::test]
    async fn mint_fails_closed_on_http_error() {
        let (url, _) = mock_admin(MintReply::Forbidden).await;
        let err = minter(&url).mint(&policy(600)).await.unwrap_err();
        assert!(err.contains("403"), "error should carry the status: {err}");
        assert!(err.contains("policy rejected"), "carries detail: {err}");
    }

    #[tokio::test]
    async fn mint_fails_closed_when_admin_unreachable() {
        // Reserved port with no listener.
        let err = minter("http://127.0.0.1:1")
            .mint(&policy(600))
            .await
            .unwrap_err();
        assert!(err.contains("POST"), "spawn-aborting mint error: {err}");
    }

    #[tokio::test]
    async fn provision_for_spawn_materializes_home_env_and_grant() {
        let dir = tempfile::tempdir().unwrap();
        let (url, _) = mock_admin(MintReply::Token("tok-1")).await;
        let job_dir = dir.path().join("jobs").join("j1");
        let p = provision_for_spawn(&minter(&url), &policy(900), &job_dir)
            .await
            .unwrap();

        let home = job_dir.join("home");
        assert!(home.join(".config").join("gh").is_dir());

        let lookup = |name: &str| {
            p.env
                .iter()
                .find(|v| v.name == name)
                .map(|v| v.value.clone())
                .unwrap_or_else(|| panic!("env {name} missing"))
        };
        assert_eq!(lookup("HALTER_TOKEN"), "tok-1");
        assert_eq!(lookup("HALTER_URL"), "http://127.0.0.1:9090");
        assert_eq!(lookup("HOME"), home.to_string_lossy());
        assert_eq!(
            lookup("GH_CONFIG_DIR"),
            home.join(".config").join("gh").to_string_lossy()
        );

        match &p.home_grant {
            Grant::Dir(d) => {
                assert_eq!(d.path, home.to_string_lossy());
                assert_eq!(d.access, Access::ReadWrite);
            }
            Grant::File(_) | Grant::WorkingDir(_) => panic!("home grant must be a Dir grant"),
        }
    }

    fn caps() -> CapabilitySpec {
        CapabilitySpec {
            network: NetworkPolicy::Block(BlockNetwork {}),
            grants: vec![Grant::Dir(DirGrant {
                path: "/ws".into(),
                access: Access::ReadWrite,
            })],
            unsafe_seatbelt_rules: None,
        }
    }

    #[test]
    fn apply_provision_absent_changes_nothing() {
        // A job with no halter policy (or no minter): capabilities pass through
        // untouched and no env is injected — identical to behavior before halter.
        let (out, env) = apply_provision(caps(), None);
        assert_eq!(out, caps());
        assert!(env.is_empty());
    }

    #[test]
    fn apply_provision_present_appends_home_grant_and_env() {
        let grant = Grant::Dir(DirGrant {
            path: "/state/jobs/j1/home".into(),
            access: Access::ReadWrite,
        });
        let env = vec![EnvVar {
            name: ENV_HALTER_TOKEN.to_string(),
            value: "tok".into(),
        }];
        let (out, injected) = apply_provision(
            caps(),
            Some(HalterProvision {
                env: env.clone(),
                home_grant: grant.clone(),
            }),
        );
        assert_eq!(out.grants.len(), caps().grants.len() + 1);
        assert_eq!(out.grants.last(), Some(&grant));
        // Existing grants and network policy are untouched.
        assert_eq!(out.grants[..out.grants.len() - 1], caps().grants[..]);
        assert_eq!(out.network, caps().network);
        assert_eq!(injected, env);
    }
}
