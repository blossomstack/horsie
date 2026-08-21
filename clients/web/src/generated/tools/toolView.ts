
import { ToolAccess } from './toolAccess';
/**
 * One selectable tool.
 */
export interface ToolView {
  /**
   * The name the model calls, and the value stored in a selection.
   */
  name: string;
  /**
   * One line, written for the person choosing — not the (much longer)
   * description the model is given.
   */
  description: string;
  access: ToolAccess;
  /**
   * Whether an unset selection includes this tool. False for the control
   * plane alone: selecting a `horsie_*` tool is how server authority is
   * granted, so it can never be granted by leaving something unset.
   */
  inDefaultSet: boolean;
}