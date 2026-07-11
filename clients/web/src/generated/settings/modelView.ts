
/**
 * One configured model alias.
 */
export interface ModelView {
  /**
   * The alias sessions select (e.g. &quot;sonnet&quot;).
   */
  alias: string;
  /**
   * Name of the provider this model routes to.
   */
  provider: string;
  /**
   * The provider&#x27;s model identifier (e.g. &quot;claude-sonnet-4-6&quot;).
   */
  modelId: string;
  maxTokens?: number;
}