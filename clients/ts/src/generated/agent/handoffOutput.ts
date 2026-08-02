
import { HandoffCall } from './handoffCall';
/**
 * Agent handed off control. A conclusion is always one call; a park (an
 */
export interface HandoffOutput {
  toolName: string;
  calls: HandoffCall[];
}