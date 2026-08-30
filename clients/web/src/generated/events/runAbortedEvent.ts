
import { Usage } from '../agent';
/**
 * A run that ended badly — cancelled, or failed anywhere in the loop. The
 * tokens it spent were spent whatever became of it, so `usage` and
 * `context_tokens` mean exactly what they do on `RunCompleteEvent`. Both are
 * zero for a run cancelled before its first provider call answered.
 *
 * Exactly one of `RunComplete` and `RunAborted` ends a run, never both, so a
 * reader may add either one's `usage` to a running total without
 * double-counting. A *turn* retried after a transient provider failure is
 * several runs and reports each — which is the truth, since each one called
 * the provider. Unlike `RunComplete` this says nothing about a turn, because
 * no turn completed.
 */
export interface RunAbortedEvent {
  messageId: string;
  usage: Usage;
  contextTokens: number;
  atMs: number;
}