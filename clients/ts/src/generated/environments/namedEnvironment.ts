
/**
 * A predefined environment, by name. Resolved and snapshotted when the session
 * is created, so editing or deleting it never re-points one that exists.
 */
export interface NamedEnvironment {
  name: string;
}