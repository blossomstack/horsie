//! Every LLM provider horsie speaks to, adapted to `LlmProvider`.
//!
//! One module per wire protocol, not one crate: the protocols are thin
//! adapters over `async-llm`'s clients, and splitting three of them across
//! three crates bought nothing but manifests. What is genuinely shared — the
//! retry envelope every adapter applies on top of its client — lives here.

use horsie_agentcore::LlmError;

pub mod anthropic;
pub mod openai;
pub mod responses;

/// How many times a stream is re-established before the turn gives up. The
/// clients own the retrying; this is the budget they are handed.
pub(crate) const MAX_STREAM_RETRIES: u32 = 6;
/// Backoff before the first retry. Each further attempt doubles it.
pub(crate) const BACKOFF_BASE_SECS: u64 = 5;
/// Bounds *idle* time between reads, not the total call.
///
/// A total `.timeout()` would kill legitimately long generations; this resets on
/// every chunk, so a slow-but-alive stream runs indefinitely while a stalled one
/// is bounded (#61 item 5).
pub(crate) const DEFAULT_READ_TIMEOUT_SECS: u64 = 120;

/// A streamed tool call's arguments, as JSON.
///
/// Shared because every protocol here delivers them the same way — as text
/// accumulated across deltas — and so fails the same way when a stream is cut
/// mid-argument. Saying so in the error is what makes that diagnosable.
fn parse_tool_input(raw: &str, tool: &str) -> Result<serde_json::Value, LlmError> {
    if raw.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(raw).map_err(|error| LlmError::ApiError {
        status: 502,
        message: format!(
            "tool call '{tool}' had unparseable input JSON ({error}); {} byte(s) received, likely a truncated stream",
            raw.len()
        ),
    })
}
