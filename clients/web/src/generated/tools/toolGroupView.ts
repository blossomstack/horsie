
import { ToolView } from './toolView';
/**
 * A named set of tools that are chosen together as often as not.
 */
export interface ToolGroupView {
  /**
   * Stable key, for test ids and any client that wants to reason about a
   * group rather than render it.
   */
  key: string;
  label: string;
  /**
   * What the group is for, shown under its heading.
   */
  description: string;
  tools: ToolView[];
}