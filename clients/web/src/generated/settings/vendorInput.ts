
import { VendorConfigInput } from './vendorConfigInput';
/**
 * A configurable vendor to persist. Replaces any vendor of the same `name`.
 */
export interface VendorInput {
  name: string;
  config: VendorConfigInput;
}