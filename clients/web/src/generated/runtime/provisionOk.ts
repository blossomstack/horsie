
/**
 * The steps that ran, in order. Named rather than counted so a failure says
 * which step failed and leaves the ones before it known to have applied.
 */
export interface ProvisionOk {
  applied: string[];
}