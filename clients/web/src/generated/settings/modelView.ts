
/**
 * One configured model alias.
 */
export interface ModelView {
  /**
   * The alias sessions select (e.g. "sonnet").
   */
  alias: string;
  /**
   * Name of the provider this model routes to.
   */
  provider: string;
  /**
   * The provider's model identifier (e.g. "claude-sonnet-4-6").
   */
  modelId: string;
  maxTokens?: number;
  /**
   * The model's total context window, in tokens. A built-in default is
   * applied for known model ids when a model is added with this omitted.
   */
  contextWindow?: number;
  /**
   * Canonical thinking-effort values this model offers, in ascending order.
   * Absent → the model exposes no thinking control.
   */
  thinkingEfforts?: string[];
  /**
   * Default applied when a session does not choose one.
   */
  thinkingEffort?: string;
  /**
   * This backend rejects a pinned `tool_choice` while thinking is enabled,
   * so thinking is disabled for those requests. Absent means false.
   */
  forcedToolsDisableThinking?: boolean;
  /**
   * Wire encoding for this model's thinking control.
   */
  thinkingDialect?: string;
  /**
   * This model can be shown images. Absent means false.
   */
  supportsImages?: boolean;
  /**
   * This model can be shown documents (PDFs). A separate flag from
   * `supports_images` because the two are genuinely different capabilities:
   * the OpenAI chat wire takes images almost everywhere but reaches PDFs by
   * another route, and Anthropic's PDF support is per model. One flag would
   * have to lie about one of them. Absent means false.
   */
  supportsDocuments?: boolean;
}