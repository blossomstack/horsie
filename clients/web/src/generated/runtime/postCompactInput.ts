
/**
 * A compaction that happened. Same matcher domain as `PreCompact`; the token
 * counts are what it bought.
 */
export interface PostCompactInput {
  trigger: string;
  tokensBefore: number;
  tokensAfter: number;
}