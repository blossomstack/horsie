
import { ToolResultInput } from './toolResultInput';
/**
 * One or more tool results delivered together — every parked call of a turn is
 * answered at once, so no `tool_use` ever reaches a provider without a result
 */
export interface ToolResultsInput {
  results: ToolResultInput[];
}