
import { Usage } from '../agent';
/**
 * Run lifecycle — message_id is a UUID for this run invocation.
 * `usage` sums every provider call this run made (tool-loop iterations
 * included) — a cost figure, not a context-size one. `context_tokens` is the
 * *last* call's prompt size alone (provider-normalized to include any cache
 * tokens) — what's actually loaded in the model's context once the run ends.
 * `at_ms` is the unix-epoch millisecond the run finished.
 */
export interface RunCompleteEvent {
  messageId: string;
  usage: Usage;
  iterations: number;
  contextTokens: number;
  atMs: number;
}