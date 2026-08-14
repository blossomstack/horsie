
import { StepFieldType } from './stepFieldType';
/**
 * One field a step's result carries beyond `outcome` and `description`.
 */
export interface StepField {
  name: string;
  kind: StepFieldType;
  /**
   * Required. An undocumented field is one the model fills in by guessing.
   */
  description: string;
  /**
   * Whether the step must always supply it. Absent → optional.
   */
  required?: boolean;
}