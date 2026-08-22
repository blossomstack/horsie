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
use horsie_models::agent::LogEntryKind;
use horsie_models::session_api::LogSearchHit;
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

/// Which end of the log a page is measured from.
///
/// A sum type rather than an `Option<u64>` plus a direction flag: `Before(None)`
/// and `After(None)` would be two spellings of the same tail, and "after,
/// anchored nowhere" has no meaning at all. Three constructors, three windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Anchor {
    /// The newest entries. Where a reader with no position starts.
    Tail,
    /// The entries immediately before this seq, newest of them last.
    Before(u64),
    /// The entries immediately after this seq.
    After(u64),
}

/// What a reader wants out of a log.
///
/// Two axes, because the log has two granularities and only one of them is an
/// entry. Which entries to return is a selection over [`LogEntryKind`]; whether
/// to keep thinking is a question about the parts *inside* an assistant
/// message, which is never an entry of its own. One enum spanning both would
/// have to claim `Thinking` is a kind of entry, and a caller selecting it would
/// get an empty page.
///
/// [`Self::everything`] is the identity, and it is what every pre-existing
/// caller passes — the browser reads a transcript to render it, and rendering
/// wants all of it.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct LogFilter {
    /// Kinds to keep. Empty means every kind, which is what makes
    /// [`Self::everything`] the default rather than a filter that returns
    /// nothing.
    pub kinds: Vec<LogEntryKind>,
    /// Drop thinking parts from the assistant messages that survive `kinds`.
    ///
    /// Not a kind, and not the same as dropping assistant messages: a model's
    /// answer is usually the point and its reasoning usually is not, so this
    /// keeps the one and discards the other. Nothing else strips parts, so an
    /// entry that ends up with none is still returned — an assistant turn that
    /// was pure thinking is a fact about the run, and silently vanishing it
    /// would leave a reader counting turns that are not there.
    pub without_thinking: bool,
}

impl LogFilter {
    /// Every entry, thinking included. The identity filter.
    #[must_use]
    pub fn everything() -> Self {
        Self::default()
    }

    /// Whether this filter would keep `entry`.
    #[must_use]
    pub fn keeps(&self, entry: &AgentLogEntry) -> bool {
        self.kinds.is_empty() || self.kinds.contains(&kind_of(entry))
    }

    /// Apply the part-level half. Cloned entries only — the log itself is never
    /// edited, so this can only ever narrow what a *reader* sees.
    fn redact(&self, entries: &mut Vec<AgentLogEntry>) {
        if !self.without_thinking {
            return;
        }
        for entry in entries {
            if let horsie_agentcore::AgentLogBody::Llm(message) = &mut entry.body {
                message
                    .parts
                    .retain(|p| !matches!(p, horsie_agentcore::ContentPart::Thinking(_)));
            }
        }
    }
}

/// Which kind of thing this entry is, for a reader choosing between them.
#[must_use]
pub fn kind_of(entry: &AgentLogEntry) -> LogEntryKind {
    match &entry.body {
        horsie_agentcore::AgentLogBody::Llm(m) => match m.role {
            horsie_agentcore::Role::User => LogEntryKind::UserMessage,
            horsie_agentcore::Role::Assistant => LogEntryKind::AssistantMessage,
            horsie_agentcore::Role::Tool => LogEntryKind::ToolResult,
        },
        horsie_agentcore::AgentLogBody::Hook(_) => LogEntryKind::Hook,
        horsie_agentcore::AgentLogBody::Lifecycle(_) => LogEntryKind::Lifecycle,
        horsie_agentcore::AgentLogBody::Compaction(_) => LogEntryKind::Compaction,
    }
}

/// Index of the entry numbered `seq`, if the log holds it.
fn index_of(log: &[AgentLogEntry], seq: u64) -> Option<usize> {
    log.binary_search_by_key(&seq, |e| e.seq).ok()
}

/// The seq of the entry with this id, if the log holds it.
///
/// A linear scan, unlike every other lookup here: ids are provider-assigned
/// (`result:{tool_call_id}`, `hook:{n}`, a model's own message id) and have no
/// order to search by. That is affordable because this resolves an anchor
/// *once* per read, and it is worth having because an id is what a caller has
/// when it is quoting something it saw in the text rather than scrolling.
#[must_use]
pub fn seq_of_id(log: &[AgentLogEntry], id: &str) -> Option<u64> {
    log.iter().find(|e| e.body.id() == Some(id)).map(|e| e.seq)
}

/// The `max` entries at `anchor` that `filter` keeps.
///
/// **The filter is applied before the window is cut**, which is the whole point
/// of it being here rather than at the caller: asking for 20 user messages
/// returns 20 user messages, not whatever survives out of the last 20 mixed
/// entries. Taking a page and then filtering it is the shape that makes a
/// reader page blind through a run, and it cannot be fixed above this function.
///
/// An unresolvable anchor yields an empty page rather than falling back to the
/// tail. The caller named an entry this log does not hold, so the honest answer
/// is nothing — handing back the tail would look like a successful scroll and
/// silently restart the window somewhere else.
///
/// Entries are always ordered oldest-first, whichever anchor was used.
#[must_use]
pub fn page(log: &[AgentLogEntry], anchor: Anchor, max: usize, filter: &LogFilter) -> LogPage {
    let empty = || LogPage {
        entries: Vec::new(),
    };
    let mut entries: Vec<AgentLogEntry> = match anchor {
        Anchor::Tail | Anchor::Before(_) => {
            let end = match anchor {
                Anchor::Before(seq) => match index_of(log, seq) {
                    Some(idx) => idx,
                    None => return empty(),
                },
                Anchor::Tail | Anchor::After(_) => log.len(),
            };
            // Backwards from the anchor, so the `max` kept are the `max`
            // *nearest* it, then flipped back into log order.
            let mut kept: Vec<AgentLogEntry> = log[..end]
                .iter()
                .rev()
                .filter(|e| filter.keeps(e))
                .take(max)
                .cloned()
                .collect();
            kept.reverse();
            kept
        }
        // Unlike `Before`, a seq the log does not hold is not an error here:
        // the log is sorted, so "everything above N" is well defined whether or
        // not N survives, and a caller resuming from a compacted-away position
        // should get what remains rather than nothing.
        Anchor::After(seq) => since(log, seq)
            .iter()
            .filter(|e| filter.keeps(e))
            .take(max)
            .cloned()
            .collect(),
    };
    filter.redact(&mut entries);
    LogPage { entries }
}

/// Everything numbered above `after` — the stream's tail, not a page.
///
/// Unbounded and unfiltered by design: this serves a live reader catching up to
/// the present, which wants every entry it has not seen and wants it in the
/// shape the log holds. [`page`] is the bounded, filtered read.
///
/// A cursor the log does not hold is not an error: the log is sorted, so
/// "everything above N" is well defined whether or not N itself survives. That
/// matters once the log can be front-trimmed — a client resuming from a
/// compacted-away position gets the entries that remain rather than nothing.
#[must_use]
pub fn since(log: &[AgentLogEntry], after: u64) -> &[AgentLogEntry] {
    let start = log.partition_point(|e| e.seq <= after);
    &log[start..]
}

/// How much of a matched entry a search hit carries.
///
/// Small on purpose. A search says *where to read*; one that answered with
/// whole entries would cost the same context as the paging it exists to avoid.
const SNIPPET_CHARS: usize = 200;

/// Entries whose text contains `needle`, oldest first, at most `max`.
///
/// Case-insensitive substring, not a regex: a regex needs its own failure mode
/// (a caller's bad pattern becomes this function's error) and its own denial-of-
/// service story (a catastrophic backtrack on a log of thousands of entries).
/// Neither is worth it for a tool whose job is "find where you said X".
#[must_use]
pub fn search(
    log: &[AgentLogEntry],
    needle: &str,
    filter: &LogFilter,
    max: usize,
) -> Vec<LogSearchHit> {
    let needle = needle.to_lowercase();
    log.iter()
        .filter(|e| filter.keeps(e))
        .filter_map(|e| {
            let text = entry_text(e);
            let at = text.to_lowercase().find(&needle)?;
            Some(LogSearchHit {
                seq: e.seq,
                at_ms: e.at_ms,
                kind: kind_of(e),
                snippet: snippet(&text, at),
            })
        })
        .take(max)
        .collect()
}

/// Everything in an entry a search should look at, flattened.
///
/// Tool call *inputs* are included as their JSON: "which turn ran that
/// command" is one of the questions a run is searched for, and the command is
/// only ever in the input.
fn entry_text(entry: &AgentLogEntry) -> String {
    use horsie_agentcore::{AgentLogBody, ContentPart};
    match &entry.body {
        AgentLogBody::Llm(m) => m
            .parts
            .iter()
            .map(|part| match part {
                ContentPart::Text(p) => p.text.clone(),
                ContentPart::Thinking(p) => p.text.clone(),
                ContentPart::ToolResult(p) => p.output.clone(),
                ContentPart::SubAgentResult(p) => p.text.clone(),
                ContentPart::ToolCall(p) => format!("{} {}", p.name, p.input),
            })
            .collect::<Vec<_>>()
            .join("\n"),
        // Serialized rather than matched arm by arm: these carry a dozen
        // variants between them and a search that silently skipped the ones
        // nobody enumerated would be worse than one that reads their JSON.
        AgentLogBody::Hook(h) => serde_json::to_string(&h.record).unwrap_or_default(),
        AgentLogBody::Lifecycle(l) => serde_json::to_string(l).unwrap_or_default(),
        AgentLogBody::Compaction(c) => serde_json::to_string(c).unwrap_or_default(),
    }
}

/// `SNIPPET_CHARS` of `text` centred on the match, with ellipses where it was
/// cut. Char-indexed throughout, so a multi-byte boundary cannot panic.
fn snippet(text: &str, at: usize) -> String {
    let chars: Vec<char> = text.chars().collect();
    // `at` is a byte offset from `find`; convert it to a char offset.
    let at = text[..at].chars().count();
    let start = at.saturating_sub(SNIPPET_CHARS / 2);
    let end = (start + SNIPPET_CHARS).min(chars.len());
    let body: String = chars[start..end].iter().collect();
    format!(
        "{}{body}{}",
        if start > 0 { "…" } else { "" },
        if end < chars.len() { "…" } else { "" }
    )
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
    use horsie_agentcore::{AgentLogBody, ContentPart, Message, Role};

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

    /// A page with the identity filter — what every window test wants, and what
    /// keeps the filtering tests below visibly about the filter.
    fn all(log: &[AgentLogEntry], anchor: Anchor, max: usize) -> LogPage {
        page(log, anchor, max, &LogFilter::everything())
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
    fn before_returns_the_window_ending_just_before_the_cursor() {
        let log = fixture(0..10);
        assert_eq!(
            seqs(&all(&log, Anchor::Before(5), 3).entries),
            vec![2, 3, 4]
        );
    }

    #[test]
    fn the_tail_anchor_returns_the_newest_entries() {
        let log = fixture(0..10);
        assert_eq!(seqs(&all(&log, Anchor::Tail, 3).entries), vec![7, 8, 9]);
    }

    #[test]
    fn before_clamps_rather_than_underflowing_at_the_start() {
        let log = fixture(0..10);
        assert_eq!(seqs(&all(&log, Anchor::Before(2), 50).entries), vec![0, 1]);
    }

    #[test]
    fn an_unknown_before_cursor_returns_an_empty_page() {
        let log = fixture(0..10);
        assert!(all(&log, Anchor::Before(999), 3).entries.is_empty());
    }

    /// The bounded forward page, as opposed to [`since`]: a caller walking a
    /// run forwards gets `max` entries at a time and can keep going from the
    /// last seq it saw.
    #[test]
    fn after_returns_a_bounded_window_past_the_cursor() {
        let log = fixture(0..10);
        assert_eq!(seqs(&all(&log, Anchor::After(2), 3).entries), vec![3, 4, 5]);
        assert!(all(&log, Anchor::After(9), 3).entries.is_empty());
    }

    #[test]
    fn since_is_everything_past_the_cursor() {
        let log = fixture(0..5);
        assert_eq!(seqs(since(&log, 2)), vec![3, 4]);
        assert!(since(&log, 4).is_empty());
        assert_eq!(seqs(since(&log, 99)), Vec::<u64>::new());
    }

    /// Guards the reason `seq` is stored rather than implied by index: a
    /// front-trimmed log must still resolve cursors against the numbers it
    /// actually holds, not against positions in the surviving slice.
    #[test]
    fn cursors_resolve_against_seq_not_position() {
        let log = fixture(100..110);
        assert_eq!(
            seqs(&all(&log, Anchor::Before(105), 2).entries),
            vec![103, 104]
        );
        assert_eq!(seqs(since(&log, 107)), vec![108, 109]);
    }

    /// A client resuming from a position the log no longer holds gets what
    /// remains, rather than the empty answer `Before` gives. The two anchors
    /// differ deliberately: scrolling back to a missing entry is a failed
    /// lookup, but reading forward from one is not.
    #[test]
    fn reading_after_a_trimmed_away_cursor_yields_what_survives() {
        let log = fixture(100..105);
        assert_eq!(seqs(since(&log, 42)), vec![100, 101, 102, 103, 104]);
        assert_eq!(
            seqs(&all(&log, Anchor::After(42), 3).entries),
            vec![100, 101, 102]
        );
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
        assert!(all(&log, Anchor::Tail, 10).entries.is_empty());
        assert!(all(&log, Anchor::Before(1), 10).entries.is_empty());
        assert!(all(&log, Anchor::After(0), 10).entries.is_empty());
        assert!(since(&log, 0).is_empty());
        assert!(replay_window(&log).0.is_empty());
    }

    // --- Filtering ---------------------------------------------------------

    fn message(seq: u64, role: Role, parts: Vec<ContentPart>) -> AgentLogEntry {
        AgentLogEntry {
            seq,
            at_ms: 1_000 + seq,
            body: AgentLogBody::Llm(Message {
                id: format!("m{seq}"),
                role,
                parts,
                created_at_ms: 1_000 + seq,
                started_at_ms: None,
            }),
        }
    }

    fn text(body: &str) -> ContentPart {
        ContentPart::Text(horsie_agentcore::TextPart {
            text: body.to_string(),
        })
    }

    fn thinking(body: &str) -> ContentPart {
        ContentPart::Thinking(horsie_agentcore::ThinkingPart {
            text: body.to_string(),
            signature: None,
        })
    }

    /// A run shaped like a real one: one user message per turn, buried under
    /// the assistant messages and tool results that answer it.
    fn noisy_run(turns: u64) -> Vec<AgentLogEntry> {
        let mut log = Vec::new();
        let mut seq = 0;
        for turn in 0..turns {
            log.push(message(seq, Role::User, vec![text(&format!("ask {turn}"))]));
            seq += 1;
            for _ in 0..8 {
                log.push(message(
                    seq,
                    Role::Assistant,
                    vec![thinking("hmm"), text("working")],
                ));
                seq += 1;
                log.push(message(
                    seq,
                    Role::Tool,
                    vec![ContentPart::ToolResult(horsie_agentcore::ToolResultPart {
                        tool_call_id: format!("tc{seq}"),
                        output: "output".to_string(),
                        is_error: false,
                    })],
                ));
                seq += 1;
            }
        }
        log
    }

    /// The whole reason the filter lives in this function. Filtering a page
    /// after it is cut would answer this with one entry, because the last 20 of
    /// a real run hold at most one user message — and no amount of work above
    /// this function could fix that.
    #[test]
    fn the_filter_is_applied_before_the_window_is_cut() {
        let log = noisy_run(5);
        let only_user = LogFilter {
            kinds: vec![LogEntryKind::UserMessage],
            without_thinking: false,
        };

        let page = page(&log, Anchor::Tail, 20, &only_user);
        assert_eq!(
            page.entries.len(),
            5,
            "asking for 20 user messages must return every user message there \
             is, not whatever survives out of the last 20 mixed entries"
        );

        // And to be sure the fixture really is the hostile case:
        let unfiltered = all(&log, Anchor::Tail, 20);
        assert_eq!(
            unfiltered
                .entries
                .iter()
                .filter(|e| kind_of(e) == LogEntryKind::UserMessage)
                .count(),
            1,
            "the last 20 entries of this run hold one user message, which is \
             what filtering-after-paging would have returned"
        );
    }

    #[test]
    fn an_empty_kind_list_keeps_everything() {
        let log = noisy_run(1);
        assert_eq!(
            all(&log, Anchor::Tail, 100).entries.len(),
            log.len(),
            "the default filter is the identity, not one that returns nothing"
        );
    }

    #[test]
    fn several_kinds_are_a_union_not_an_intersection() {
        let log = noisy_run(2);
        let filter = LogFilter {
            kinds: vec![LogEntryKind::UserMessage, LogEntryKind::ToolResult],
            without_thinking: false,
        };
        let kinds: Vec<LogEntryKind> = page(&log, Anchor::Tail, 100, &filter)
            .entries
            .iter()
            .map(kind_of)
            .collect();
        assert!(kinds.contains(&LogEntryKind::UserMessage));
        assert!(kinds.contains(&LogEntryKind::ToolResult));
        assert!(!kinds.contains(&LogEntryKind::AssistantMessage));
    }

    /// Thinking is a part, not an entry: dropping it must leave the assistant
    /// message — and what it actually said — in place.
    #[test]
    fn dropping_thinking_keeps_the_message_that_carried_it() {
        let log = vec![message(
            0,
            Role::Assistant,
            vec![thinking("secret reasoning"), text("the answer")],
        )];
        let filter = LogFilter {
            kinds: vec![],
            without_thinking: true,
        };
        let page = page(&log, Anchor::Tail, 10, &filter);
        assert_eq!(page.entries.len(), 1, "the entry itself survives");
        let AgentLogBody::Llm(m) = &page.entries[0].body else {
            panic!("expected an llm entry")
        };
        assert_eq!(m.parts.len(), 1);
        assert!(matches!(&m.parts[0], ContentPart::Text(t) if t.text == "the answer"));
    }

    /// An assistant turn that was *only* thinking still counts as a turn. If it
    /// vanished, a reader counting turns would find fewer than happened.
    #[test]
    fn a_message_left_with_no_parts_is_still_returned() {
        let log = vec![message(0, Role::Assistant, vec![thinking("just musing")])];
        let filter = LogFilter {
            kinds: vec![],
            without_thinking: true,
        };
        assert_eq!(page(&log, Anchor::Tail, 10, &filter).entries.len(), 1);
    }

    /// The log itself is never edited — redaction happens on the cloned page.
    /// If it did not, one filtered read would strip thinking out of the state
    /// the provider replays from.
    #[test]
    fn redaction_does_not_touch_the_log() {
        let log = vec![message(0, Role::Assistant, vec![thinking("keep me")])];
        let filter = LogFilter {
            kinds: vec![],
            without_thinking: true,
        };
        let _ = page(&log, Anchor::Tail, 10, &filter);
        let AgentLogBody::Llm(m) = &log[0].body else {
            panic!("expected an llm entry")
        };
        assert_eq!(m.parts.len(), 1, "the source log still holds its thinking");
    }

    #[test]
    fn every_body_maps_to_a_kind() {
        let log = noisy_run(1);
        assert_eq!(kind_of(&log[0]), LogEntryKind::UserMessage);
        assert_eq!(kind_of(&log[1]), LogEntryKind::AssistantMessage);
        assert_eq!(kind_of(&log[2]), LogEntryKind::ToolResult);
    }

    // --- Id anchoring and search -------------------------------------------

    #[test]
    fn an_id_resolves_to_the_seq_that_carries_it() {
        let log = fixture(100..110);
        assert_eq!(seq_of_id(&log, "m105"), Some(105));
        assert_eq!(seq_of_id(&log, "nope"), None);
    }

    /// The round trip the tool relies on: an id seen in a page anchors the next
    /// one, in either direction.
    #[test]
    fn an_id_anchor_pages_in_both_directions() {
        let log = fixture(0..10);
        let seq = seq_of_id(&log, "m5").unwrap();
        assert_eq!(seqs(&all(&log, Anchor::Before(seq), 2).entries), vec![3, 4]);
        assert_eq!(seqs(&all(&log, Anchor::After(seq), 2).entries), vec![6, 7]);
    }

    #[test]
    fn search_finds_positions_and_is_case_insensitive() {
        let log = vec![
            message(0, Role::User, vec![text("Deploy the thing")]),
            message(1, Role::Assistant, vec![text("done")]),
            message(2, Role::User, vec![text("redeploy please")]),
        ];
        let hits = search(&log, "DEPLOY", &LogFilter::everything(), 10);
        assert_eq!(hits.iter().map(|h| h.seq).collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(hits[0].kind, LogEntryKind::UserMessage);
    }

    /// Search honours the same filter a read does, so "where did the person ask
    /// about X" does not also return the model repeating X back.
    #[test]
    fn search_respects_the_kind_filter() {
        let log = vec![
            message(0, Role::User, vec![text("deploy")]),
            message(1, Role::Assistant, vec![text("deploy")]),
        ];
        let filter = LogFilter {
            kinds: vec![LogEntryKind::UserMessage],
            without_thinking: false,
        };
        let hits = search(&log, "deploy", &filter, 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].seq, 0);
    }

    /// A tool call's input is where the command lives, and "which turn ran
    /// that" is one of the questions a run is searched for.
    #[test]
    fn search_looks_inside_a_tool_call_input() {
        let log = vec![message(
            0,
            Role::Assistant,
            vec![ContentPart::ToolCall(horsie_agentcore::ToolCallPart {
                id: "tc0".to_string(),
                name: "bash".to_string(),
                input: serde_json::json!({"command": "cargo test --workspace"}),
            })],
        )];
        assert_eq!(
            search(&log, "cargo test", &LogFilter::everything(), 10).len(),
            1
        );
    }

    /// A snippet is a pointer, not the entry. Answering with whole entries
    /// would cost the context that paging exists to save.
    #[test]
    fn a_snippet_is_capped_and_marks_where_it_was_cut() {
        let long = "x".repeat(1_000) + "needle" + &"y".repeat(1_000);
        let log = vec![message(0, Role::User, vec![text(&long)])];
        let hit = &search(&log, "needle", &LogFilter::everything(), 10)[0];
        assert!(
            hit.snippet.chars().count() <= SNIPPET_CHARS + 2,
            "a snippet may exceed the cap only by its two ellipses, got {}",
            hit.snippet.chars().count()
        );
        assert!(hit.snippet.contains("needle"));
        assert!(hit.snippet.starts_with('…') && hit.snippet.ends_with('…'));
    }

    /// Char-indexed, not byte-indexed: slicing a multi-byte string on a byte
    /// boundary panics, and a transcript is full of them.
    #[test]
    fn a_snippet_survives_multibyte_text() {
        let long = "日".repeat(300) + "needle" + &"本".repeat(300);
        let log = vec![message(0, Role::User, vec![text(&long)])];
        let hit = &search(&log, "needle", &LogFilter::everything(), 10)[0];
        assert!(hit.snippet.contains("needle"));
    }

    #[test]
    fn search_stops_at_max() {
        let log: Vec<AgentLogEntry> = (0..50)
            .map(|seq| message(seq, Role::User, vec![text("same")]))
            .collect();
        assert_eq!(search(&log, "same", &LogFilter::everything(), 5).len(), 5);
    }
}
