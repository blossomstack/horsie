//! Git parses the helper's stdout, and its stderr reaches the model as part of
//! the tool that ran the git command. A subscriber would put every crate's
//! events on both — `RUST_LOG=debug` alone is enough.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Write;
use std::process::{Command, Stdio};

/// The refused server drives the http client the noise comes from.
#[test]
fn a_credential_fetch_says_nothing_on_either_stream() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_horsie-runtime"))
        .arg("git-credential")
        .arg("get")
        .env("RUST_LOG", "trace")
        .env(horsie_models::ENV_SERVER_URL, "http://127.0.0.1:1")
        .env(horsie_models::ENV_CONNECT_TOKEN, "x")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"protocol=https\nhost=github.com\npath=o/r.git\n\n")
        .unwrap();

    let out = child.wait_with_output().unwrap();
    assert_eq!(
        String::from_utf8_lossy(&out.stdout),
        "",
        "git parses this stream; the helper has no credential to offer here"
    );
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "",
        "this stream is read back to the model as part of the tool's output"
    );
}
