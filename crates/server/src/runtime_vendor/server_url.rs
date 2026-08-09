//! The HTTP address a cloud vendor's runtimes reach this server at.
//!
//! Derived from the vendor's configured `callback_url` rather than configured
//! twice. That URL is already "the address a machine reaches this server on" —
//! it is what the runtime dials — so a second setting for the same fact would
//! be one more thing to get wrong, and wrong here means a runtime that boots
//! and then silently cannot fetch anything.
//!
//! **A vendor that supplies no address at all was the status quo, and it was a
//! bug.** Neither cloud vendor ever set the bundle base URL, so
//! `provision_plugins` returned before its first fetch and plugin bundles never
//! worked on Fly or velos — silently, because bundle fetching is best-effort.
//! The GitHub credential helper needs the same address, so supplying it is no
//! longer optional and the old gap closes with it.

/// `ws(s)://host/api/runtime/connect` → `http(s)://host`.
///
/// Only the scheme and the connect path are touched. Anything else in the
/// authority — a port, a path prefix a reverse proxy adds — is preserved,
/// because a deployment that mounts horsie under a prefix reaches its artifacts
/// under that same prefix.
#[must_use]
pub fn http_base_of(callback_url: &str) -> String {
    let swapped = if let Some(rest) = callback_url.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = callback_url.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        callback_url.to_string()
    };
    swapped
        .trim_end_matches('/')
        .trim_end_matches("/api/runtime/connect")
        .trim_end_matches('/')
        .to_string()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_callback_url_becomes_the_http_base_its_runtimes_fetch_from() {
        assert_eq!(
            http_base_of("wss://horsie.example.com/api/runtime/connect"),
            "https://horsie.example.com"
        );
        assert_eq!(
            http_base_of("ws://horsie:8080/api/runtime/connect"),
            "http://horsie:8080"
        );
    }

    /// A bare origin is already the base. The settings layer appends the connect
    /// path on save, but a stored row from before that, or one written by hand,
    /// must not lose its host to an over-eager trim.
    #[test]
    fn a_bare_origin_survives_unchanged() {
        assert_eq!(http_base_of("ws://horsie:8080"), "http://horsie:8080");
        assert_eq!(
            http_base_of("wss://h.example.com/"),
            "https://h.example.com"
        );
    }

    /// A reverse proxy that mounts horsie under a prefix serves its artifacts
    /// under that prefix too, so only the connect path itself comes off.
    #[test]
    fn a_path_prefix_is_preserved() {
        assert_eq!(
            http_base_of("wss://edge.example.com/horsie/api/runtime/connect"),
            "https://edge.example.com/horsie"
        );
    }

    /// An unexpected scheme is passed through rather than mangled: it is a
    /// misconfiguration, and a wrong-but-plausible URL is harder to diagnose
    /// than the operator's own string coming back at them.
    #[test]
    fn an_unknown_scheme_is_left_alone() {
        assert_eq!(http_base_of("http://horsie:8080"), "http://horsie:8080");
    }
}
