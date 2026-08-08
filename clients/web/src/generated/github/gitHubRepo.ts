
/**
 * One repository visible to the App installation.
 */
export interface GitHubRepo {
  /**
   * "owner/name".
   */
  fullName: string;
  private: boolean;
  defaultBranch: string;
}