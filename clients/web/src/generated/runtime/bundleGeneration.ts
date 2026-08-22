
/**
 * An authored bundle's generation, bumped on every save. There is no upstream
 * whose bytes could differ, and the server can always re-render the package
 * from its own tables, so a counter is identity enough.
 */
export interface BundleGeneration {
  generation: number;
}