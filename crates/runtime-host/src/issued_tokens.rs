//! The dial tokens a vendor process has handed out, and which runtime each one
//! names.
//!
//! A vendor process used to verify dial-backs by re-computing an HMAC over a
//! secret it minted for itself. That worked, but it made the token meaningless
//! to anyone else: the server had never seen the secret, so it could not accept
//! a dial-back from one of these runtimes and could not authenticate anything
//! else the runtime later asked for. The server mints now, and this vendor no
//! longer holds a secret to verify against.
//!
//! It does not need one. It is the party that handed the token out, so
//! recognising one is a lookup rather than a computation — and that is strictly
//! stronger than the check it replaces. An HMAC accepts *any* well-formed token
//! the secret signs, including one minted for a runtime this vendor never
//! started. A token this vendor never issued is not merely unsigned here; it is
//! unknown.
//!
//! The token is opaque: nothing here parses it or cares what shape it has.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Table {
    /// The lookup a dial-back performs.
    by_token: HashMap<String, String>,
    /// The inverse, kept only so issuing a fresh token for a runtime can retire
    /// the one it replaces.
    by_runtime: HashMap<String, String>,
}

/// Tokens this vendor process has issued, keyed both ways.
#[derive(Default)]
pub struct IssuedTokens {
    table: Mutex<Table>,
}

impl IssuedTokens {
    #[must_use]
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Record that `token` names `runtime_id`, retiring whatever token that
    /// runtime had before.
    ///
    /// Retiring matters: a revive mints a fresh token for the same runtime, and
    /// leaving the previous one live would mean a token that leaked once
    /// outlived every rotation the server performed.
    pub fn issue(&self, token: &str, runtime_id: &str) {
        let Ok(mut table) = self.table.lock() else {
            return;
        };
        if let Some(previous) = table
            .by_runtime
            .insert(runtime_id.to_string(), token.to_string())
        {
            table.by_token.remove(&previous);
        }
        table
            .by_token
            .insert(token.to_string(), runtime_id.to_string());
    }

    /// The runtime this token names, or `None` if this vendor never issued it.
    #[must_use]
    pub fn resolve(&self, token: &str) -> Option<String> {
        self.table.lock().ok()?.by_token.get(token).cloned()
    }

    /// Forget a runtime's token. Called when the runtime goes away, so a halted
    /// runtime's token cannot be replayed to register as it.
    pub fn revoke_runtime(&self, runtime_id: &str) {
        let Ok(mut table) = self.table.lock() else {
            return;
        };
        if let Some(token) = table.by_runtime.remove(runtime_id) {
            table.by_token.remove(&token);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn only_a_token_this_vendor_issued_resolves() {
        let issued = IssuedTokens::new();
        issued.issue("tok-a", "rt-1");
        assert_eq!(issued.resolve("tok-a").as_deref(), Some("rt-1"));
        assert_eq!(issued.resolve("tok-b"), None);
    }

    /// A revive mints a fresh token for the same runtime. The old one has to
    /// stop working, or a token that leaked once is good forever however many
    /// times the server re-mints.
    #[test]
    fn reissuing_for_one_runtime_retires_its_previous_token() {
        let issued = IssuedTokens::new();
        issued.issue("old", "rt-1");
        issued.issue("new", "rt-1");
        assert_eq!(issued.resolve("old"), None);
        assert_eq!(issued.resolve("new").as_deref(), Some("rt-1"));
    }

    #[test]
    fn two_runtimes_keep_their_own_tokens() {
        let issued = IssuedTokens::new();
        issued.issue("tok-a", "rt-1");
        issued.issue("tok-b", "rt-2");
        assert_eq!(issued.resolve("tok-a").as_deref(), Some("rt-1"));
        assert_eq!(issued.resolve("tok-b").as_deref(), Some("rt-2"));
    }

    #[test]
    fn a_revoked_runtimes_token_stops_resolving() {
        let issued = IssuedTokens::new();
        issued.issue("tok-a", "rt-1");
        issued.revoke_runtime("rt-1");
        assert_eq!(issued.resolve("tok-a"), None);
        // Revoking one runtime leaves every other alone.
        issued.issue("tok-b", "rt-2");
        issued.revoke_runtime("rt-1");
        assert_eq!(issued.resolve("tok-b").as_deref(), Some("rt-2"));
    }

    #[test]
    fn an_empty_token_is_never_recognised() {
        // A dial-back with no bearer at all must not resolve to whatever the
        // map happens to hold under the empty string.
        let issued = IssuedTokens::new();
        issued.issue("tok-a", "rt-1");
        assert_eq!(issued.resolve(""), None);
    }
}
