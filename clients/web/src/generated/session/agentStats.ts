
import { UsageView } from './usageView';
/**
 * One agent's banked numbers: what it spent, what everything under it spent,
 * and how full its context is.
 *
 * Banked rather than live — every figure is folded from an event the session
 * journaled — which is what lets a whole roster report them without waking a
 * single agent. `contextTokens` is therefore as of the end of that agent's
 * last turn, not as of now.
 */
export interface AgentStats {
  /**
   * Tokens this agent alone has spent.
   */
  usage: UsageView;
  /**
   * `usage` plus every agent below it: the subagents it spawned, the sub
   * sessions branched from it, the steps of the workflows it invoked, and
   * everything below those.
   */
  subtreeUsage: UsageView;
  /**
   * Tokens its context held at the end of its last turn.
   */
  contextTokens: number;
  /**
   * The window the model it runs allows, when that model is configured.
   * Absent when it is not — a number with no denominator is not a gauge.
   */
  contextWindow?: number;
}