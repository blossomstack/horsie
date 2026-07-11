
import { VelosInput } from './velosInput';
/**
 * Kind-tagged vendor config input. Add a variant per new vendor kind.
 */
export type VendorConfigInput =
  | { kind: "Velos"; value: VelosInput };