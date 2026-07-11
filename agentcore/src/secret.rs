use serde::Deserialize;

/// A secret value — an API key, bearer token, or similar credential — that
/// must never leak into logs, error messages, or debug output. `Debug` and
/// `Display` always print a fixed redaction marker; the only way to read the
/// wrapped value is the explicit [`Secret::expose`] call, so a leak shows up
/// as a deliberate call site rather than an incidental `{:?}`/`{}`.
#[derive(Clone, PartialEq, Eq, Deserialize)]
#[serde(transparent)]
pub struct Secret(String);

impl Secret {
    /// The wrapped value, for the one place that legitimately needs it: handing
    /// the credential to an HTTP client or API builder.
    pub fn expose(&self) -> &str {
        &self.0
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl From<String> for Secret {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for Secret {
    fn from(value: &str) -> Self {
        Self(value.to_string())
    }
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(\"***\")")
    }
}

impl std::fmt::Display for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("***")
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn debug_never_contains_the_value() {
        let secret = Secret::from("sk-super-secret-value");
        assert!(!format!("{secret:?}").contains("sk-super-secret-value"));
    }

    #[test]
    fn display_never_contains_the_value() {
        let secret = Secret::from("sk-super-secret-value");
        assert!(!format!("{secret}").contains("sk-super-secret-value"));
    }

    #[test]
    fn expose_returns_the_wrapped_value() {
        let secret = Secret::from("sk-super-secret-value");
        assert_eq!(secret.expose(), "sk-super-secret-value");
    }

    #[test]
    fn deserializes_from_a_plain_json_string() {
        let secret: Secret = serde_json::from_str("\"sk-abc\"").unwrap();
        assert_eq!(secret.expose(), "sk-abc");
    }

    #[test]
    fn is_empty_reflects_the_wrapped_value() {
        assert!(Secret::from("").is_empty());
        assert!(!Secret::from("x").is_empty());
    }
}
