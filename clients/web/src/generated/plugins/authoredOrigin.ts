
/**
 * A bundle whose source is rows in this server's database.
 */
export interface AuthoredOrigin {
  /**
   * Bumped on every save. Names one revision of the plugin's rows, and is
   * what a runtime fetches the rendered package by.
   */
  generation: number;
}