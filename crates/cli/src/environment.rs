//! Turning the environment flags every run-something command takes into the
//! `EnvironmentSpec` the server requires.
//!
//! One helper rather than one per command, so `workflow run`, `agent invoke`
//! and the routine commands cannot drift on what `--environment` means.

use crate::error::CliError;
use horsie_models::environments::{EnvironmentSpec, NamedEnvironment, RuntimeEnvironment};
use horsie_models::session_api::RepoConfig;

/// `--environment <name>` or `--vendor <name> [--repo …]`, exactly one.
///
/// The two flag shapes are the two variants of the union. Naming both is
/// ambiguous rather than a merge: a predefined environment already carries a
/// vendor, and silently letting one win would make the other flag a no-op the
/// user cannot see.
pub fn environment_from_flags(
    environment: Option<String>,
    vendor: Option<String>,
    repos: Vec<String>,
) -> Result<EnvironmentSpec, CliError> {
    match (environment, vendor) {
        (Some(_), Some(_)) => Err(CliError::Config(
            "--environment and --vendor are alternatives; pass one".into(),
        )),
        (Some(_), None) if !repos.is_empty() => Err(CliError::Config(
            "--repo goes with --vendor; a named environment carries its own repos".into(),
        )),
        (Some(name), None) => Ok(EnvironmentSpec::Named(NamedEnvironment { name })),
        (None, Some(vendor)) => Ok(EnvironmentSpec::Runtime(RuntimeEnvironment {
            vendor,
            repos: (!repos.is_empty()).then(|| {
                repos
                    .into_iter()
                    .map(|url| RepoConfig {
                        url,
                        git_ref: None,
                        dir: None,
                    })
                    .collect()
            }),
        })),
        (None, None) => Err(CliError::Config(
            "pass --environment <name> or --vendor <name>: a session has to say where it runs"
                .into(),
        )),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_name_becomes_the_named_variant() {
        let spec = environment_from_flags(Some("staging".into()), None, vec![]).unwrap();
        assert_eq!(
            spec,
            EnvironmentSpec::Named(NamedEnvironment {
                name: "staging".into()
            })
        );
    }

    #[test]
    fn a_vendor_becomes_the_runtime_variant_and_carries_its_repos() {
        let spec = environment_from_flags(
            None,
            Some("fly".into()),
            vec!["https://github.com/o/api".into()],
        )
        .unwrap();
        match spec {
            EnvironmentSpec::Runtime(r) => {
                assert_eq!(r.vendor, "fly");
                assert_eq!(r.repos.unwrap()[0].url, "https://github.com/o/api");
            }
            EnvironmentSpec::Named(_) => panic!("expected the runtime variant"),
        }
    }

    #[test]
    fn a_vendor_with_no_repos_sends_none_rather_than_an_empty_list() {
        let spec = environment_from_flags(None, Some("local".into()), vec![]).unwrap();
        match spec {
            EnvironmentSpec::Runtime(r) => assert!(r.repos.is_none()),
            EnvironmentSpec::Named(_) => panic!("expected the runtime variant"),
        }
    }

    #[test]
    fn naming_both_is_ambiguous_and_naming_neither_is_incomplete() {
        assert!(environment_from_flags(Some("s".into()), Some("fly".into()), vec![]).is_err());
        assert!(environment_from_flags(None, None, vec![]).is_err());
    }

    #[test]
    fn a_repo_on_a_named_environment_is_refused() {
        assert!(
            environment_from_flags(Some("s".into()), None, vec!["https://x/y".into()]).is_err()
        );
    }
}
