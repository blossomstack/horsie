
/**
 * Rename a space and/or change its description. Omitted fields are unchanged.
 */
export interface MemorySpaceUpdateInput {
  name?: string;
  description?: string;
}