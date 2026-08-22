//! Cursors and windows over an agent's log.
//!
//! The log is a `Vec<AgentLogEntry>` sorted by `seq` — by construction, since
//! the fold is the only thing that appends and it hands numbers out in order.
//! That is what lets every lookup here be a binary search rather than the scan
//! the id-keyed transcript needed.
//!
//! Nothing in this module touches an actor or a journal. It is a pure function
//! over a slice, which is the reason it lives in its own file: the interesting
//! part is a table of cases, and a table of cases wants isolated tests.

use horsie_agentcore::AgentLogEntry;
use serde::{Deserialize, Serialize};

/// A client's position in an agent's stream.
///
/// Two numbers because there are two kinds of thing to be positioned in: log
/// entries, which are durable and numbered by the fold, and the deltas that
/// have arrived since the newest entry, which are not durable at all.
///
/// Deltas restart at 1 after every entry. That is what makes a stale delta
/// position *detectable* rather than merely wrong: a client claiming more
/// deltas than the agent currently holds can only have been talking to a run
/// that has since restarted. A single flat counter across both kinds would
/// reissue the same numbers to different content after a crash, and nothing
/// could tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cursor {
    pub entry_seq: u64,
    pub delta_seq: usize,
}

impl Cursor {
    /// Parse `"99"` or `"99.50"`. `None` for anything else — a malformed cursor
    /// is treated as absent rather than as position zero, so a client is
    /// re-seeded instead of being silently rewound to the start.
    #[must_use]
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.split_once('.') {
            None => Some(Self {
                entry_seq: raw.parse().ok()?,
                delta_seq: 0,
            }),
            Some((entry, delta)) => Some(Self {
                entry_seq: entry.parse().ok()?,
                delta_seq: delta.parse().ok()?,
            }),
        }
    }
}

impl std::fmt::Display for Cursor {
    /// The SSE `id:` for this position, so `Last-Event-ID` on reconnect *is*
    /// the `after=` a fresh request would carry.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.delta_seq == 0 {
            write!(f, "{}", self.entry_seq)
        } else {
            write!(f, "{}.{}", self.entry_seq, self.delta_seq)
        }
    }
}

/// One window of a log.
///
/// No `has_more` in either direction. Fewer entries than asked for means there
/// are no more, which is the same fact without a second way to say it — and
/// without the forward-page ambiguity `has_more_after` produced, where a
/// backfill could not tell "nothing newer" from "nothing newer *in this
/// direction*".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogPage {
    pub entries: Vec<AgentLogEntry>,
}

impl LogPage {
    /// Just the LLM messages in this window, for callers reasoning about the
    /// session rather than the log. Hook and lifecycle entries are
    /// skipped, so a page of `n` entries may yield fewer than `n` messages.
    pub fn messages(&self) -> impl Iterator<Item = &horsie_agentcore::Message> {
        self.entries.iter().filter_map(|e| match &e.body {
            horsie_agentcore::AgentLogBody::Llm(m) => Some(m),
            // A compaction boundary is not a message and never becomes one
            // here: this is a window over what was *said*, and callers
            // reasoning about the prompt go through
            // `AgentState::prompt_messages`, which is the only thing that knows
            // where a boundary moves the start.
            horsie_agentcore::AgentLogBody::Hook(_)
            | horsie_agentcore::AgentLogBody::Lifecycle(_)
            | horsie_agentcore::AgentLogBody::Compaction(_) => None,
        })
    }
}

/// Index of the entry numbered `seq`, if the log holds it.
fn index_of(log: &[AgentLogEntry], seq: u64) -> Option<usize> {
    log.binary_search_by_key(&seq, |e| e.seq).ok()
}

/// The `max` entries ending just before `before`, or the tail when `before` is
/// absent.
///
/// An unresolvable `before` yields an empty page rather than falling back to
/// the tail. The caller named an entry this log does not hold, so the honest
/// answer is nothing — handing back the tail would look like a successful
/// scroll-back and silently restart the window somewhere else.
#[must_use]
pub fn page_before(log: &[AgentLogEntry], before: Option<u64>, max: usize) -> LogPage {
    let end = match before {
        None => log.len(),
        Some(seq) => match index_of(log, seq) {
            Some(idx) => idx,
            None => {
                return LogPage {
                    entries: Vec::new(),
                };
            }
        },
    };
    let start = end.saturating_sub(max);
    LogPage {
        entries: log[start..end].to_vec(),
    }
}

/// Everything numbered above `after`.
///
/// A cursor the log does not hold is not an error here: the log is sorted, so
/// "everything above N" is well defined whether or not N itself survives. That
/// matters once the log can be front-trimmed — a client resuming from a
/// compacted-away position gets the entries that remain rather than nothing.
#[must_use]
pub fn page_after(log: &[AgentLogEntry], after: u64) -> &[AgentLogEntry] {
    let start = log.partition_point(|e| e.seq <= after);
    &log[start..]
}

/// How many entries a cursorless connect replays at most.
///
/// A session that has run for months should not resend its whole history every
/// time someone opens it. The client is told when the cap bit and pages back
/// with `before=` for the rest.
pub const REPLAY_CAP: usize = 5_000;

/// The newest `REPLAY_CAP` entries — what a client with no cursor at all gets —
/// and whether anything was left behind.
///
/// Returns the truncation as a flag rather than leaving it to be inferred from
/// the first entry's `seq`: a front-trimmed log has no seq 0 either, and that
/// case wants the opposite answer.
#[must_use]
pub fn replay_window(log: &[AgentLogEntry]) -> (&[AgentLogEntry], bool) {
    if log.len() <= REPLAY_CAP {
        return (log, false);
    }
    (&log[log.len() - REPLAY_CAP..], true)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use horsie_agentcore::{AgentLogBody, Message, Role};

    fn entry(seq: u64) -> AgentLogEntry {
        AgentLogEntry {
            seq,
            at_ms: 1_000 + seq,
            body: AgentLogBody::Llm(Message {
                id: format!("m{seq}"),
                role: Role::User,
                parts: vec![],
                created_at_ms: 1_000 + seq,
                started_at_ms: None,
            }),
        }
    }

    fn fixture(seqs: std::ops::Range<u64>) -> Vec<AgentLogEntry> {
        seqs.map(entry).collect()
    }

    fn seqs(entries: &[AgentLogEntry]) -> Vec<u64> {
        entries.iter().map(|e| e.seq).collect()
    }

    #[test]
    fn a_cursor_round_trips_in_both_forms() {
        assert_eq!(
            Cursor::parse("99"),
            Some(Cursor {
                entry_seq: 99,
                delta_seq: 0
            })
        );
        assert_eq!(
            Cursor::parse("99.50"),
            Some(Cursor {
                entry_seq: 99,
                delta_seq: 50
            })
        );
        assert_eq!(
            Cursor {
                entry_seq: 99,
                delta_seq: 0
            }
            .to_string(),
            "99"
        );
        assert_eq!(
            Cursor {
                entry_seq: 99,
                delta_seq: 50
            }
            .to_string(),
            "99.50"
        );
    }

    /// Absent, not zero. A client handed a corrupted `Last-Event-ID` must be
    /// re-seeded rather than silently rewound to the start of the session.
    #[test]
    fn a_malformed_cursor_is_none_rather_than_zero() {
        assert_eq!(Cursor::parse("nonsense"), None);
        assert_eq!(Cursor::parse("99.nonsense"), None);
        assert_eq!(Cursor::parse(""), None);
        assert_eq!(Cursor::parse("-1"), None);
    }

    #[test]
    fn page_before_returns_the_window_ending_just_before_the_cursor() {
        let log = fixture(0..10);
        assert_eq!(seqs(&page_before(&log, Some(5), 3).entries), vec![2, 3, 4]);
    }

    #[test]
    fn page_before_with_no_cursor_returns_the_tail() {
        let log = fixture(0..10);
        assert_eq!(seqs(&page_before(&log, None, 3).entries), vec![7, 8, 9]);
    }

    #[test]
    fn page_before_clamps_rather_than_underflowing_at_the_start() {
        let log = fixture(0..10);
        assert_eq!(seqs(&page_before(&log, Some(2), 50).entries), vec![0, 1]);
    }

    #[test]
    fn an_unknown_before_cursor_returns_an_empty_page() {
        let log = fixture(0..10);
        assert!(page_before(&log, Some(999), 3).entries.is_empty());
    }

    #[test]
    fn page_after_is_everything_past_the_cursor() {
        let log = fixture(0..5);
        assert_eq!(seqs(page_after(&log, 2)), vec![3, 4]);
        assert!(page_after(&log, 4).is_empty());
        assert_eq!(seqs(page_after(&log, 99)), Vec::<u64>::new());
    }

    /// Guards the reason `seq` is stored rather than implied by index: a
    /// front-trimmed log must still resolve cursors against the numbers it
    /// actually holds, not against positions in the surviving slice.
    #[test]
    fn cursors_resolve_against_seq_not_position() {
        let log = fixture(100..110);
        assert_eq!(
            seqs(&page_before(&log, Some(105), 2).entries),
            vec![103, 104]
        );
        assert_eq!(seqs(page_after(&log, 107)), vec![108, 109]);
    }

    /// A client resuming from a position the log no longer holds gets what
    /// remains, rather than the empty answer `before` gives. The two cursors
    /// differ deliberately: scrolling back to a missing anchor is a failed
    /// lookup, but streaming forward from one is not.
    #[test]
    fn page_after_a_trimmed_away_cursor_yields_what_survives() {
        let log = fixture(100..105);
        assert_eq!(seqs(page_after(&log, 42)), vec![100, 101, 102, 103, 104]);
    }

    /// A cursorless connect gets the newest window, not the whole history — a
    /// session that has run for months must not resend all of it on every open.
    #[test]
    fn a_replay_is_capped_to_the_newest_window() {
        let short = fixture(0..10);
        let (entries, more) = replay_window(&short);
        assert_eq!(entries.len(), 10);
        assert!(!more, "a log under the cap is replayed whole");

        let long: Vec<AgentLogEntry> = (0..(REPLAY_CAP as u64 + 25)).map(entry).collect();
        let (entries, more) = replay_window(&long);
        assert_eq!(entries.len(), REPLAY_CAP);
        assert!(more, "the caller must learn the cap bit, not infer it");
        assert_eq!(
            entries.first().unwrap().seq,
            25,
            "the window is the newest entries, so paging back from its first \
             seq reaches what was left out"
        );
        assert_eq!(entries.last().unwrap().seq, REPLAY_CAP as u64 + 24);
    }

    #[test]
    fn an_empty_log_answers_every_cursor_with_nothing() {
        let log: Vec<AgentLogEntry> = vec![];
        assert!(page_before(&log, None, 10).entries.is_empty());
        assert!(page_before(&log, Some(1), 10).entries.is_empty());
        assert!(page_after(&log, 0).is_empty());
        assert!(replay_window(&log).0.is_empty());
    }
}
