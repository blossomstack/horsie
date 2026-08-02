
import { ToolResultInput } from './toolResultInput';
/**
 * One or more tool results delivered together — every parked call of a turn is
 */
export interface ToolResultsInput {
  results: ToolResultInput[];
}