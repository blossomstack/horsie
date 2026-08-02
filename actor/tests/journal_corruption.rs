// The journal itself is under test here, so it is called directly.
#![allow(clippy::disallowed_methods)]
//! #61 item 13: mid-file journal corruption silently truncates replay.
//!
//! `decode_after` treats an undecodable line as a stop condition and `break`s
//! (`actor/src/file_journal.rs:136-146`), returning a short-but-clean stream.
//! `recover` (`actor/src/runtime.rs:120-127`) cannot distinguish that from a
//! genuinely short log, so it adopts the truncated prefix as the true state —
//! while the surviving tail events are still physically in the file, and the actor
//! then appends *after* them. Permanent split-brain, plus a silent shift of every
//! SSE sequence id past the corruption point.

#![cfg(all(feature = "file-journal", feature = "test-util"))]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

use futures_util::StreamExt;
use horsie_actor::testkit::write_corrupt_journal;
use horsie_actor::{FileJournal, Journal, PersistenceId};
use std::time::Duration;

#[tokio::test]
#[ignore = "red: #61 item 13 — corrupt journal line truncates replay silently instead of erroring"]
async fn replay_surfaces_an_error_when_a_journal_line_is_corrupt() {
    tokio::time::timeout(Duration::from_secs(10), async {
        let dir = tempfile::tempdir().unwrap();
        let pid = PersistenceId::new("t", "corrupt");
        // Three batches; the middle line is unreadable, the third is intact.
        write_corrupt_journal(
            dir.path(),
            &pid,
            &[vec![vec![1]], vec![vec![2]], vec![vec![3]]],
            1,
        )
        .unwrap();

        let journal = FileJournal::new(dir.path());
        let items: Vec<_> = journal.replay(&pid, 0).await.collect().await;

        assert!(
            items.iter().any(std::result::Result::is_err),
            "corruption must surface as an error, got a clean {}-event stream: {items:?}",
            items.len()
        );
    })
    .await
    .expect("test timed out");
}
