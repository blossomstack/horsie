
/**
 * Create a memory space. `name` must be a slug: lowercase letters, digits,
 */
export interface MemorySpaceCreateInput {
  name: string;
  description?: string;
}