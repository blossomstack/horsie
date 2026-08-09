//! `horsie-runtime git-credential` — a git credential helper that mints a
//! GitHub token per operation.
//!
//! Git invokes this with `get`, `store` or `erase` and a key/value block on
//! stdin. Only `get` does anything: it asks the server for a token scoped to
//! the repository git is about to talk to, and prints it back in git's format.
//!
//! **Why a helper rather than a token in the environment.** A GitHub App
//! installation token lives about an hour, and nothing could renew the one the
//! server used to bake into `GITHUB_TOKEN` at create time — a runtime older
//! than that held a dead credential, and a hibernated machine came back with
//! the same one. Minting at the moment of use makes the lifetime irrelevant:
//! nothing holds a token longer than a single git command.
//!
//! It also makes `git push` work. The clone this replaced passed its token as a
//! one-shot `http.extraHeader`, deliberately leaving nothing in `.git/config` —
//! so every later git operation in the workspace had no credential at all.
//!
//! **Failure is silent and successful.** Printing nothing and exiting 0 is how
//! a helper says "I have no credentials for this", which is exactly right when
//! GitHub is not connected: a public repository still clones. Exiting non-zero
//! would make git treat it as broken rather than empty-handed.

/// The repository git wants credentials for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialQuery {
    pub host: String,
    pub path: String,
}

/// Parse git's key/value block into the repository it names.
///
/// `None` whenever this helper has nothing to say: a non-HTTPS protocol, a host
/// that is not github.com, or — importantly — a request with no `path`.
///
/// A missing path is not a detail to paper over. Git only sends one when
/// `credential.useHttpPath` is set, and without it the server would be asked
/// for a token scoped to nothing in particular. Minting an
/// every-repo-in-the-installation token because a config line was missing is
/// exactly the failure this design exists to avoid, so it declines instead.
#[must_use]
pub fn query_of(input: &str) -> Option<CredentialQuery> {
    let mut protocol = None;
    let mut host = None;
    let mut path = None;
    for line in input.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key {
            "protocol" => protocol = Some(value),
            "host" => host = Some(value),
            "path" => path = Some(value),
            _ => {}
        }
    }
    if protocol? != "https" {
        return None;
    }
    let host = host?;
    if host != "github.com" {
        return None;
    }
    let path = path?.trim_matches('/').trim_end_matches(".git");
    if path.is_empty() {
        return None;
    }
    Some(CredentialQuery {
        host: host.to_string(),
        path: path.to_string(),
    })
}

/// Git's reply block for a token.
#[must_use]
fn reply(token: &str) -> String {
    // The username is fixed for an App installation token; GitHub ignores it
    // beyond requiring it to be present.
    format!("username=x-access-token\npassword={token}\n")
}

/// Ask the server for a token for `query`.
async fn mint(server: &str, bearer: &str, query: &CredentialQuery) -> Option<String> {
    let url = format!(
        "{}/api/runtime/github-credential",
        server.trim_end_matches('/')
    );
    let response = reqwest::Client::builder()
        .build()
        .ok()?
        .get(url)
        // Built by the client rather than interpolated: a repository name is
        // not ours to assume is URL-safe.
        .query(&[("host", &query.host), ("path", &query.path)])
        .bearer_auth(bearer)
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    let token = response.text().await.ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Answer one `get`, or `None` when there is nothing to answer with.
pub async fn respond(input: &str, server: &str, bearer: &str) -> Option<String> {
    let query = query_of(input)?;
    mint(server, bearer, &query).await.as_deref().map(reply)
}

/// The subcommand body: read stdin, answer a `get`, ignore everything else.
///
/// `store` and `erase` are deliberately no-ops. There is nothing to store —
/// the next operation mints again — and nothing to erase.
pub async fn run(operation: &str) {
    if operation != "get" {
        return;
    }
    let (Ok(server), Ok(bearer)) = (
        std::env::var(horsie_models::ENV_SERVER_URL),
        std::env::var(horsie_models::ENV_CONNECT_TOKEN),
    ) else {
        return;
    };
    let mut input = String::new();
    if std::io::Read::read_to_string(&mut std::io::stdin(), &mut input).is_err() {
        return;
    }
    if let Some(answer) = respond(&input, &server, &bearer).await {
        print!("{answer}");
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_github_https_request_names_its_repo() {
        let q = query_of("protocol=https\nhost=github.com\npath=owner/repo.git\n").unwrap();
        assert_eq!(q.host, "github.com");
        assert_eq!(
            q.path, "owner/repo",
            "the .git suffix is not part of the name"
        );
    }

    /// Git omits `path` unless `credential.useHttpPath` is set. Answering
    /// anyway would mean asking for a token scoped to no repository in
    /// particular, which is the one thing this whole path exists to avoid.
    #[test]
    fn a_request_without_a_path_cannot_be_scoped_and_is_declined() {
        assert_eq!(query_of("protocol=https\nhost=github.com\n"), None);
        assert_eq!(query_of("protocol=https\nhost=github.com\npath=\n"), None);
        assert_eq!(query_of("protocol=https\nhost=github.com\npath=/\n"), None);
    }

    #[test]
    fn a_host_or_protocol_this_helper_does_not_serve_is_declined() {
        assert_eq!(
            query_of("protocol=https\nhost=gitlab.com\npath=o/r.git\n"),
            None
        );
        assert_eq!(
            query_of("protocol=http\nhost=github.com\npath=o/r.git\n"),
            None
        );
        assert_eq!(query_of("host=github.com\npath=o/r.git\n"), None);
    }

    /// A subdomain must not be read as github.com. `codeload.github.com` and
    /// `github.com.evil.test` are both "not github.com", and an `ends_with`
    /// check would have accepted the second.
    #[test]
    fn a_lookalike_host_is_not_github() {
        for host in ["github.com.evil.test", "notgithub.com", "evil-github.com"] {
            assert_eq!(
                query_of(&format!("protocol=https\nhost={host}\npath=o/r.git\n")),
                None,
                "{host} must not be treated as github.com"
            );
        }
    }

    #[test]
    fn unknown_keys_and_blank_lines_are_ignored() {
        // Git sends a trailing blank line, and adds keys over time.
        let q = query_of("protocol=https\nhost=github.com\npath=o/r\nwwwauth[]=Basic\n\n").unwrap();
        assert_eq!(q.path, "o/r");
    }

    #[test]
    fn the_reply_is_gits_key_value_format() {
        assert_eq!(
            reply("ghs-abc"),
            "username=x-access-token\npassword=ghs-abc\n"
        );
    }
}
