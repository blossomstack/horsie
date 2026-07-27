
/**
 * A memory space as shown in the web UI.
 */
export interface MemorySpaceView {
  name: string;
  description: string;
  /**
   * How many memories the space holds.
   */
  memoryCount: number;
}