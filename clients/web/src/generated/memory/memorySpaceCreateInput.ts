
/**
 * Create a memory space. `name` must be a slug: lowercase letters, digits,
 * '.', '_' and '-', starting with a letter or digit.
 */
export interface MemorySpaceCreateInput {
  name: string;
  description?: string;
}