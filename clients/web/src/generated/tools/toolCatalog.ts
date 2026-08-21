
import { ToolGroupView } from './toolGroupView';
/**
 * Every built-in tool this server offers, grouped.
 */
export interface ToolCatalog {
  groups: ToolGroupView[];
}