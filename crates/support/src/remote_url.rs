//! What horsie is willing to dereference on a user's behalf.
//!
//! Three fields reach the network from a value someone typed: a model
//! provider's base URL, a git clone URL, and an MCP server URL. None of them
//! was parsed at all — `mcp/store.rs` was two `is_empty()` checks — so every
//! one accepted whatever it was given and handed it to reqwest or to `git`.
//!
//! **What this deliberately does not do is filter addresses.** `localhost` and
//! RFC1918 stay reachable, because that is how a self-hosted horsie reaches a
//! local model server: `http://localhost:11434/v1` is Ollama's documented base
//! URL, and a LAN MCP server is an ordinary setup. Blocking them would be a
//! real regression for the primary deployment shape in exchange for closing a
//! hole that today needs an authenticated admin who already owns the box.
//!
//! That trade would change if these ever became fields a less-privileged party
//! could set, since an address policy is the right answer once setting one is
//! not already an administrative act. It is not the right answer today, and
//! adding it now would break every self-hoster.
//!
//! So the rules here are the ones that hold either way: a scheme horsie
//! actually speaks, and nothing that stops being a URL and starts being an
//! argument.

use url::Url;

/// The only schemes horsie will fetch over.
const HTTP_SCHEMES: [&str; 2] = ["http", "https"];

/// Validate a URL horsie will fetch: a provider base URL, or an MCP endpoint.
///
/// # Errors
/// If the value is empty, unparseable, has no host, or uses a scheme other
/// than `http`/`https`.
pub fn check_fetch_url(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("a URL is required".into());
    }
    let parsed = Url::parse(trimmed).map_err(|e| format!("not a valid URL: {e}"))?;
    if !HTTP_SCHEMES.contains(&parsed.scheme()) {
        return Err(format!(
            "unsupported URL scheme '{}': only http and https are fetched",
            parsed.scheme()
        ));
    }
    if !parsed.has_host() {
        return Err("the URL has no host".into());
    }
    Ok(())
}

/// Schemes `git clone` may use.
///
/// `file` is here on purpose. Cloning a repo on the server's own disk is a
/// legitimate self-host setup — a marketplace served from the same box — and
/// it is how this repo's own plugin suite builds its fixtures. It reads only
/// what an authenticated admin can already read, which is the same bar as the
/// LAN addresses this module deliberately does not filter.
///
/// `ssh` and `git` are not: horsie manages no SSH identity, so an `ssh://`
/// remote would silently borrow whatever key the server process happens to
/// have, against a host the operator chose.
const GIT_SCHEMES: [&str; 3] = ["http", "https", "file"];

/// Validate a URL handed to `git clone`.
///
/// Different from [`check_fetch_url`] because `git` is a *process* and the URL
/// becomes one of its arguments. Two things matter here that do not for
/// reqwest, and both turn a URL into code:
///
/// - A value beginning with `-` is read by git as an option rather than a
///   remote, and `--upload-pack=<cmd>` runs `<cmd>`.
/// - `ext::` is a git transport that executes its argument as a shell command
///   outright.
///
/// The leading dash is checked separately because such a value never reaches
/// [`Url::parse`] as something with a scheme at all. Note that `ext::sh -c …`
/// *does* parse — as scheme `ext` — so the allowlist catches it.
///
/// # Errors
/// If the value is empty, begins with `-`, is unparseable, or uses a scheme
/// outside [`GIT_SCHEMES`].
pub fn check_git_url(raw: &str) -> Result<(), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Err("a git URL is required".into());
    }
    if trimmed.starts_with('-') {
        return Err("a git URL cannot begin with '-': git would read it as an option".into());
    }
    let parsed = Url::parse(trimmed).map_err(|e| format!("not a valid git URL: {e}"))?;
    if !GIT_SCHEMES.contains(&parsed.scheme()) {
        return Err(format!(
            "unsupported git URL scheme '{}': only http, https and file are cloned \
             (ssh, git and ext remotes are not)",
            parsed.scheme()
        ));
    }
    // Deliberately no `has_host` check: `file:///srv/repo` has no host and is
    // exactly the local-clone case above.
    Ok(())
}

/// Replace the userinfo in every URL inside `text` with `***`.
///
/// A git remote can legitimately carry a credential — `https://user:token@host/repo`
/// is how a private marketplace is cloned without an SSH identity — and horsie
/// stores it because it needs it again on every refresh. What it must not do is
/// hand it back: the URL was returned verbatim by `GET /api/plugins` and
/// `/api/marketplaces`, rendered in the marketplace rows, and echoed inside
/// `git clone failed: …` bodies, so a token typed once into a form ended up on
/// screen, in the API and in an error message.
///
/// Text rather than a URL, because the worst offender is `git`'s stderr, which
/// is prose with a URL somewhere in it. The scan is deliberately narrow: from a
/// `://` to the next `@`, stopping at any character that cannot appear in
/// userinfo, so an `@` later in a path or query is not mistaken for one.
pub fn redact_url_credentials(text: &str) -> String {
    const MARK: &str = "://";
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(MARK) {
        let (head, tail) = rest.split_at(at + MARK.len());
        out.push_str(head);
        // Userinfo runs to the first `@`, and cannot contain any of these.
        let end = tail.find(['@', '/', '?', '#', ' ', '\t', '\n', '"', '\'']);
        match end {
            Some(i) if tail.as_bytes().get(i) == Some(&b'@') => {
                out.push_str("***@");
                rest = &tail[i + 1..];
            }
            _ => rest = tail,
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn credentials_are_stripped_from_a_url() {
        assert_eq!(
            redact_url_credentials("https://user:token@github.com/o/r.git"),
            "https://***@github.com/o/r.git"
        );
        assert_eq!(
            redact_url_credentials("https://token@github.com/o/r.git"),
            "https://***@github.com/o/r.git"
        );
    }

    #[test]
    fn a_url_with_no_credential_is_untouched() {
        for url in [
            "https://github.com/o/r.git",
            "http://localhost:11434/v1",
            "file:///srv/repo",
            // An `@` in the path is not userinfo.
            "https://example.com/o/r@v2.git",
            "https://example.com/search?q=a@b",
        ] {
            assert_eq!(redact_url_credentials(url), url, "{url}");
        }
    }

    #[test]
    fn every_url_in_a_line_of_prose_is_covered() {
        let stderr = "fatal: could not read from https://u:p@a.example/x.git; \
                      tried https://u2:p2@b.example/y.git too";
        let out = redact_url_credentials(stderr);
        assert!(!out.contains("u:p@"), "{out}");
        assert!(!out.contains("u2:p2@"), "{out}");
        assert_eq!(out.matches("***@").count(), 2, "{out}");
    }

    #[test]
    fn text_with_no_url_survives_intact() {
        for s in ["", "no urls here", "a@b.example is an address", "://"] {
            assert_eq!(redact_url_credentials(s), s, "{s:?}");
        }
    }

    #[test]
    fn ordinary_http_urls_are_accepted() {
        for url in [
            "https://api.anthropic.com",
            "http://example.com/mcp",
            "https://example.com:8443/path?q=1",
        ] {
            assert!(check_fetch_url(url).is_ok(), "{url} should be accepted");
        }
    }

    #[test]
    fn localhost_and_lan_stay_reachable() {
        // Load-bearing, and the reason there is no address policy here. Ollama
        // documents exactly the first one; breaking these to close an SSRF that
        // needs an admin who already owns the box is a bad trade. Anyone
        // tempted to "harden" this should read the module docs first.
        for url in [
            "http://localhost:11434/v1",
            "http://127.0.0.1:1234/v1",
            "http://192.168.1.50:8080/mcp",
            "http://10.0.0.5:3000",
            "http://[::1]:11434/v1",
        ] {
            assert!(check_fetch_url(url).is_ok(), "{url} must stay reachable");
        }
    }

    #[test]
    fn non_http_schemes_are_refused() {
        for url in [
            "file:///etc/passwd",
            "ftp://example.com/x",
            "data:text/plain,hello",
            "javascript:alert(1)",
        ] {
            assert!(check_fetch_url(url).is_err(), "{url} should be refused");
        }
    }

    #[test]
    fn junk_is_refused_rather_than_dialled() {
        for url in ["", "   ", "not a url", "https://", "http://"] {
            assert!(check_fetch_url(url).is_err(), "{url:?} should be refused");
        }
    }

    #[test]
    fn a_git_url_cannot_become_a_command() {
        // The two that turn a URL into code. `--upload-pack=<cmd>` runs
        // `<cmd>`; `ext::` executes its argument outright.
        assert!(check_git_url("--upload-pack=touch /tmp/pwned").is_err());
        assert!(check_git_url("-u foo").is_err());
        assert!(check_git_url("ext::sh -c 'touch /tmp/pwned'").is_err());
    }

    #[test]
    fn transports_horsie_has_no_identity_for_are_refused() {
        // horsie manages no SSH key, so an ssh remote would borrow whatever the
        // server process happens to hold, against a host someone else chose.
        assert!(check_git_url("ssh://git@example.com/repo.git").is_err());
        assert!(check_git_url("git://example.com/repo.git").is_err());
        // A bare path is not a URL and never had a scheme to allow.
        assert!(check_git_url("/etc/passwd").is_err());
        assert!(check_git_url("git@github.com:o/r.git").is_err());
    }

    #[test]
    fn ordinary_git_remotes_are_accepted() {
        for url in [
            "https://github.com/blossomstack/horsie.git",
            "http://git.internal.example/repo.git",
            "https://user:token@github.com/o/r.git",
            // A repo on the server's own disk: a real self-host setup, and what
            // this repo's plugin fixtures clone.
            "file:///srv/marketplaces/mine",
        ] {
            assert!(check_git_url(url).is_ok(), "{url} should be accepted");
        }
    }

    #[test]
    fn the_dash_check_names_the_actual_problem() {
        let e = check_git_url("--upload-pack=x").unwrap_err();
        assert!(e.contains("option"), "unhelpful message: {e}");
    }
}
