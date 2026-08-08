
/**
 * Rename a space and/or change its description. Omitted fields are unchanged.
 * Renaming carries the space's memories across.
 */
export interface MemorySpaceUpdateInput {
  name?: string;
  description?: string;
}