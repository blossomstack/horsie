use crate::{error::LlmError, events::EventSink, tool::ToolSpec};
use async_trait::async_trait;
use horsie_models::agent::{ContentPart, Message, Usage};

pub struct CompletionRequest<'a> {
    pub messages: &'a [Message],
    pub system: Option<String>,
    pub tools: Vec<ToolSpec>,
    pub tool_choice: ToolChoice,
    pub max_tokens: Option<u32>,
    /// Canonical thinking effort for this session; `None` sends no control.
    pub thinking_effort: Option<crate::thinking::ThinkingEffort>,
    /// Which conversation this turn belongs to: the same value for every turn of
    /// one conversation, and different across conversations.
    ///
    /// Required rather than optional so it cannot be quietly omitted. A provider
    /// that groups requests by it (the Responses wire sends it as
    /// `prompt_cache_key`) gets no second chance if it is missing — the effect
    /// of a wrong value is a silently colder cache, never an error. Providers
    /// with no use for it ignore it.
    pub conversation_id: &'a str,
    /// The bytes for every artifact the messages reference, already base64.
    ///
    /// Messages carry only ids — see `ArtifactRef` — so this is what turns a
    /// reference into something a model can actually look at. It is built once
    /// per turn, for exactly the artifacts in the prompt window, and handed in
    /// here rather than fetched by a provider: a provider gets no store handle,
    /// no callback and does no I/O, which is what keeps all three of them pure
    /// functions over their input.
    ///
    /// Base64 rather than raw bytes because every wire format wants base64 —
    /// Anthropic's `source.data`, OpenAI's data URL — so encoding once at
    /// hydration is both cheaper than doing it per provider and keeps the
    /// dependency out of them.
    ///
    /// An id missing from here is not an error: it is how a model with no
    /// vision capability is served, and every provider falls back to
    /// `ArtifactRef::omitted_text`.
    pub artifacts: &'a ArtifactBytes,
}

/// Where an agent gets artifact bytes from.
///
/// Implemented by the server over its artifact service. It sits here rather
/// than being resolved by the caller once per turn because a tool can produce
/// an artifact *mid-run* — a screenshot taken between two provider calls — and
/// the very next call has to be able to show it.
///
/// **This is also the one place vision gating lives.** An implementation that
/// knows the session's model cannot see images returns nothing, and every
/// provider then renders `ArtifactRef::omitted_text` instead. No provider
/// needs a capability flag.
///
/// Returns only what it could resolve, with no error: an artifact whose bytes
/// cannot be loaded is omitted rather than failing the turn. The model is told
/// the artifact was withheld, which keeps the conversation coherent, where a
/// failed turn would lose the user's message entirely.
#[async_trait]
pub trait ArtifactSource: Send + Sync {
    /// Base64 for each of `ids` that may be shown, keyed by id.
    async fn resolve(&self, ids: &[String]) -> std::collections::HashMap<String, String>;
}

/// Artifact bytes for one turn, keyed by artifact id, base64-encoded.
#[derive(Debug, Clone, Default)]
pub struct ArtifactBytes(std::collections::HashMap<String, String>);

impl ArtifactBytes {
    #[must_use]
    pub fn new(map: std::collections::HashMap<String, String>) -> Self {
        Self(map)
    }

    /// The base64 for an id, or `None` when it was not hydrated — a model that
    /// cannot be shown this artifact, or one deliberately withheld.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&str> {
        self.0.get(id).map(String::as_str)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Merge in newly-resolved bytes.
    pub fn extend(&mut self, more: std::collections::HashMap<String, String>) {
        self.0.extend(more);
    }

    /// A shared empty set, for the calls that deliberately show no artifacts —
    /// a summariser, and any test that is not about them.
    ///
    /// Borrowed from a `OnceLock` rather than built per call because
    /// `CompletionRequest` holds a reference: `&Default::default()` borrows a
    /// temporary, which will not outlive a function that returns the request.
    #[must_use]
    pub fn empty() -> &'static ArtifactBytes {
        static EMPTY: std::sync::OnceLock<ArtifactBytes> = std::sync::OnceLock::new();
        EMPTY.get_or_init(ArtifactBytes::default)
    }
}

#[derive(Debug, Clone)]
pub struct CompletionResponse {
    pub parts: Vec<ContentPart>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxTokens,
}

#[derive(Debug, Clone)]
pub enum ToolChoice {
    Auto,
    Any,
    Required(String),
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn model_id(&self) -> &str;

    /// Perform a completion. `message_id` is the agent-assigned ID for the assistant
    /// message being generated; providers should tag any streaming events they emit with it.
    async fn complete(
        &self,
        request: CompletionRequest<'_>,
        message_id: &str,
        events: &dyn EventSink,
    ) -> Result<CompletionResponse, LlmError>;
}
