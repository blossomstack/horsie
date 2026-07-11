
/**
 * A provider to persist. Replaces any provider of the same `name`.
 */
export interface ProviderInput {
  name: string;
  kind: string;
  baseUrl?: string;
  apiKeyEnv?: string;
  /**
   * New inline key. Omit to keep the existing stored key; &quot;&quot; to clear.
   */
  apiKey?: string;
}