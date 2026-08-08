
import { HandoffCall } from './handoffCall';
/**
 * Agent handed off control. A conclusion is always one call; a park (an
 * optional handoff, e.g. `ask_user`) may be several issued in the same turn,
 * and all of them are answered together
 */
export interface HandoffOutput {
  toolName: string;
  calls: HandoffCall[];
}