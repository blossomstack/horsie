//! `GET /api/runtime/github-credential` — a GitHub token, minted per git
//! operation for one repository.
//!
//! The runtime's git credential helper is the only caller. It presents the dial
//! token, names the repository git is about to talk to, and gets back a token
//! scoped to exactly that repository.
//!
//! **This replaces baking `GITHUB_TOKEN` into the runtime environment.** An App
//! installation token lives about an hour and nothing could renew the one the
//! server put in the spec at create time, so a runtime older than that held a
//! dead credential and a resumed machine came back with the same one. Minting
//! here makes the lifetime irrelevant — nothing holds a token beyond a single
//! git command — and makes revocation immediate, because this check runs on
//! every operation rather than once, an hour ago.
//!
//! **The scope is the asking runtime's own repositories.** The requested
//! repository has to appear in a `git_checkout` provision step of the runtime
//! git is running inside; anything else is refused. Both the session to ask and
//! the runtime to ask about come out of the dial token, so the runtime never
//! has to tell us who it is and could not lie about it if it tried.
//!
//! That pairing is load-bearing, and was briefly missing. A runtime used to
//! share its session's id, so this route looked the session up by the runtime
//! id and found it. Sessions can own several runtimes now and each gets an id
//! of its own, which left the lookup asking for a session under an id no
//! session has ever had — refusing every mint, including the ones that were the
//! point. The token carries the session because nothing else can put it back.
//!
//! Like `/api/runtime/connect`, this sits outside `require_auth`: a runtime
//! holds no session credential, and the dial token is the whole authentication.

use crate::http::AppState;
use crate::http::error::Api;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, header};
use serde::Deserialize;

#[derive(Deserialize)]
pub struct CredentialRequest {
    host: String,
    path: String,
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

/// The `owner/repo` a `git_checkout` url names, if it is a github.com https
/// url. Kept next to its only caller: this is the comparison the authorization
/// check makes, not a general-purpose parser.
fn github_repo_of(url: &str) -> Option<String> {
    let rest = url.strip_prefix("https://")?.strip_prefix("github.com/")?;
    let repo = rest.trim_end_matches('/').trim_end_matches(".git");
    // `owner/repo` exactly — not a deeper path, which would compare equal to a
    // repository the session never asked for.
    let (owner, name) = repo.split_once('/')?;
    (!owner.is_empty() && !name.is_empty() && !name.contains('/')).then(|| repo.to_string())
}

pub async fn github_credential(
    State(state): State<AppState>,
    Query(request): Query<CredentialRequest>,
    headers: HeaderMap,
) -> Result<String, Api> {
    // One answer for every refusal: a stranger learns nothing about which
    // accounts, sessions or repositories exist.
    let refused = || Api::forbidden("no credential for this repository");

    let token = bearer(&headers).ok_or_else(refused)?;
    let account = horsie_support::dial_token::claimed_account(&token).ok_or_else(refused)?;
    let account = crate::projects::ProjectId::new(account);
    let secret = crate::config::dial_secret_of(&state.shared.db, &account)
        .await
        .map_err(Api::internal)?
        .ok_or_else(refused)?;
    let claims = horsie_support::dial_token::verify(&secret, &token).map_err(|_| refused())?;

    if request.host != "github.com" {
        return Err(refused());
    }
    let wanted = request.path.trim_matches('/').trim_end_matches(".git");

    let services = state.projects.get(&account).await.map_err(Api::internal)?;

    // Which session to ask, and which of its runtimes is asking. Both come out
    // of a token the runtime cannot forge, so it can neither claim another
    // session's sandbox nor lie about which of its own it is.
    let runtime = claims
        .runtime_id
        .parse::<uuid::Uuid>()
        .map(crate::sessions::spec::RuntimeId)
        .map_err(|_| refused())?;
    let checkouts = crate::http::handlers::ask(&services, |reply| {
        crate::sessions::supervisor::SessionSupervisorCommand::RuntimeCheckouts {
            id: claims.session_id.clone(),
            runtime,
            reply,
        }
    })
    .await?
    .flatten()
    .ok_or_else(refused)?;

    // Only a repository this runtime was provisioned to check out. The match is
    // against its own provision steps rather than the account's GitHub
    // installation: the installation is far wider, and a sandbox that clones
    // one repository has no business minting credentials for every other.
    let matched = checkouts
        .into_iter()
        .find(|url| github_repo_of(url).as_deref() == Some(wanted))
        .ok_or_else(refused)?;

    let minted = services
        .github
        .mint_token_for(std::slice::from_ref(&matched))
        .await
        .map_err(Api::internal)?;

    // No GitHub connection is not an error: the helper prints nothing, git
    // proceeds unauthenticated, and a public repository still clones.
    minted.ok_or_else(refused)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_github_https_url_yields_its_owner_and_name() {
        assert_eq!(
            github_repo_of("https://github.com/o/r.git").as_deref(),
            Some("o/r")
        );
        assert_eq!(
            github_repo_of("https://github.com/o/r").as_deref(),
            Some("o/r")
        );
        assert_eq!(
            github_repo_of("https://github.com/o/r/").as_deref(),
            Some("o/r")
        );
    }

    #[test]
    fn anything_that_is_not_a_github_https_repo_is_not_one() {
        for url in [
            "https://gitlab.com/o/r.git",
            "file:///tmp/x",
            "git@github.com:o/r.git",
            "https://github.com/",
            "https://github.com/o",
            "https://github.com/o/",
        ] {
            assert_eq!(github_repo_of(url), None, "{url}");
        }
    }

    /// A deeper path must not compare equal to the repository at its root: a
    /// session cloning `o/r` should not mint for a request naming `o/r/x`, and
    /// neither should a request naming `o/r` be satisfied by a step for a
    /// different repo whose url merely starts the same way.
    #[test]
    fn a_deeper_path_is_not_the_repository_at_its_root() {
        assert_eq!(github_repo_of("https://github.com/o/r/sub"), None);
    }

    #[test]
    fn a_bearer_is_read_only_when_it_is_a_non_empty_bearer() {
        let mut headers = HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer   ".parse().unwrap());
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, "Basic abc".parse().unwrap());
        assert_eq!(bearer(&headers), None);
        headers.insert(header::AUTHORIZATION, "Bearer u1.s1.tag".parse().unwrap());
        assert_eq!(bearer(&headers).as_deref(), Some("u1.s1.tag"));
    }
}
