//! In-sandbox halter self-provisioning. When the daemon minted a policy-bound
//! token for this job, it injected `HALTER_TOKEN` and `HALTER_URL` (plus a
//! synthetic `HOME`) into this process. Before the message loop starts we fetch
//! the provision doc through the halter proxy and render it into native tool
//! config under the synthetic home, so the job's stock tools (`gh` etc.) reach
//! upstreams through the halter proxy — the only network egress the sandbox
//! permits.
//!
//! Provisioning runs IN-PROCESS via the `halter_agent` library: no `halter-agent`
//! binary on `PATH`, no child process. The runtime therefore has no PATH
//! dependency for provisioning.
//!
//! Fail closed: a half-set environment, or any fetch/write failure, aborts the
//! runtime (the job fails visibly) rather than running unprovisioned.

use std::path::PathBuf;

/// Env var carrying the minted policy-bound token (daemon-injected).
pub const ENV_HALTER_TOKEN: &str = "HALTER_TOKEN";
/// Env var carrying the halter proxy-listener base URL (daemon-injected).
pub const ENV_HALTER_URL: &str = "HALTER_URL";

/// A fully-specified provisioning request: both halter vars are present along
/// with the synthetic home halter writes native tool config into. Constructed
/// only by [`setup_from_env`], so a half-set environment can never reach
/// [`run_setup`].
#[derive(Debug, PartialEq)]
pub struct HalterSetup {
    pub halter_url: String,
    pub token: String,
    pub home: PathBuf,
}

/// Decide the provisioning intent from this runtime's environment.
///
/// - both `HALTER_TOKEN` and `HALTER_URL` set → `Ok(Some(_))` (provision);
/// - neither set → `Ok(None)` (no halter, skip silently);
/// - exactly one set, or set without a `HOME` → `Err` (illegal half-provisioned
///   state; the daemon injects all three together, so this is a wiring bug and
///   the job must fail visibly).
///
/// Empty values count as unset — an empty token or URL can never provision.
pub fn setup_from_env(
    token: Option<String>,
    url: Option<String>,
    home: Option<String>,
) -> Result<Option<HalterSetup>, String> {
    match (non_empty(token), non_empty(url)) {
        (None, None) => Ok(None),
        (Some(token), Some(halter_url)) => {
            let home = non_empty(home).ok_or_else(|| {
                format!(
                    "{ENV_HALTER_TOKEN} and {ENV_HALTER_URL} are set but HOME is not — \
                     the daemon must inject a synthetic HOME alongside them"
                )
            })?;
            Ok(Some(HalterSetup {
                halter_url,
                token,
                home: PathBuf::from(home),
            }))
        }
        (Some(_), None) => Err(format!(
            "{ENV_HALTER_TOKEN} is set but {ENV_HALTER_URL} is not — \
             refusing to start half-provisioned"
        )),
        (None, Some(_)) => Err(format!(
            "{ENV_HALTER_URL} is set but {ENV_HALTER_TOKEN} is not — \
             refusing to start half-provisioned"
        )),
    }
}

/// Treat empty env values as unset.
fn non_empty(v: Option<String>) -> Option<String> {
    v.filter(|s| !s.is_empty())
}

/// Provision in-process via the `halter_agent` library: fetch the provision doc
/// through the proxy listener, then render native tool config under the
/// synthetic home. Any fetch or write failure is a hard failure (fail closed).
pub async fn run_setup(setup: &HalterSetup) -> Result<(), String> {
    let doc = halter_agent::fetch_provision(&setup.halter_url, &setup.token).await?;
    halter_agent::write_configs(&setup.home, &doc).map_err(|e| {
        format!(
            "halter: writing tool config under {}: {e}",
            setup.home.display()
        )
    })?;
    Ok(())
}

/// Entry point for the runtime main: read the env, validate the intent, and —
/// when the daemon provisioned this job for halter — fetch + write the provision
/// doc. No halter env → `Ok(())` without side effects.
pub async fn provision_from_env() -> Result<(), String> {
    match setup_from_env(
        env_var(ENV_HALTER_TOKEN),
        env_var(ENV_HALTER_URL),
        env_var("HOME"),
    )? {
        Some(setup) => run_setup(&setup).await,
        None => Ok(()),
    }
}

fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    fn s(v: &str) -> Option<String> {
        Some(v.to_string())
    }

    #[test]
    fn neither_var_set_skips_silently() {
        assert_eq!(setup_from_env(None, None, s("/home/u")).unwrap(), None);
        // Empty values count as unset.
        assert_eq!(setup_from_env(s(""), s(""), None).unwrap(), None);
    }

    #[test]
    fn both_vars_set_yields_setup() {
        let setup = setup_from_env(s("tok"), s("http://proxy"), s("/jobs/j1/home"))
            .unwrap()
            .expect("should provision");
        assert_eq!(
            setup,
            HalterSetup {
                halter_url: "http://proxy".into(),
                token: "tok".into(),
                home: PathBuf::from("/jobs/j1/home"),
            }
        );
    }

    #[test]
    fn half_set_env_is_an_error() {
        assert!(setup_from_env(s("tok"), None, s("/h")).is_err());
        assert!(setup_from_env(None, s("http://proxy"), s("/h")).is_err());
        // An empty partner value is still half-set.
        assert!(setup_from_env(s("tok"), s(""), s("/h")).is_err());
    }

    #[test]
    fn missing_home_with_halter_env_is_an_error() {
        let err = setup_from_env(s("tok"), s("http://proxy"), None).unwrap_err();
        assert!(err.contains("HOME"), "error should name HOME: {err}");
        assert!(setup_from_env(s("tok"), s("http://proxy"), s("")).is_err());
    }

    #[tokio::test]
    async fn run_setup_fails_closed_when_proxy_unreachable() {
        // Reserved port with no listener: the in-process fetch must surface a
        // spawn-aborting error rather than silently skipping provisioning.
        let setup = HalterSetup {
            halter_url: "http://127.0.0.1:1".into(),
            token: "t".into(),
            home: PathBuf::from("/h"),
        };
        let err = run_setup(&setup).await.unwrap_err();
        assert!(
            err.contains("provision request failed"),
            "fetch error surfaced: {err}"
        );
    }
}
