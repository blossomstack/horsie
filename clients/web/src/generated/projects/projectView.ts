
/**
 * One project.
 */
export interface ProjectView {
  /**
   * The scope key, and the `{project}` segment of every scoped URL.
   *
   * Opaque and stable: renaming a project does not move it, so a link keeps
   * working.
   */
  id: string;
  name: string;
  /**
   * The one project an account always has. It cannot be deleted, so a client
   * always has somewhere to send a request that names no project of its own.
   */
  isDefault: boolean;
  createdAt: string;
  updatedAt: string;
}