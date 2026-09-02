//! Selecting the rustls crypto provider, once, at process start.
//!
//! rustls 0.23 refuses to guess when more than one provider is compiled in, and
//! it refuses at *runtime*: the first TLS handshake panics with "Could not
//! automatically determine the process-level CryptoProvider". More than one is
//! compiled in here — `reqwest`'s native-roots feature pulls `ring`, while
//! `jsonschema` and `sigstore-tuf` pull `aws-lc-rs` — so every binary that
//! speaks TLS has to choose.
//!
//! Choosing explicitly rather than arranging for exactly one to be enabled: the
//! feature resolution that would leave one winner is not stable against adding
//! a dependency, and the failure it produces is a panic on the first `https://`
//! or `wss://` call rather than anything a compiler or a unit test would catch.
//! A dependency bump should not be able to break TLS.
//!
//! `ring` rather than `aws-lc-rs` because `aws-lc-rs` wants a C toolchain, and
//! keeping the build free of C is most of the reason the TLS stack is rustls at
//! all.

/// Install `ring` as the process-wide rustls provider. Idempotent, and safe to
/// call from any binary's `main` before it opens a connection.
///
/// A second call — or a caller that installed one first — is not an error:
/// `install_default` reports the slot was already filled, and either way the
/// postcondition this function promises holds.
pub fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}
