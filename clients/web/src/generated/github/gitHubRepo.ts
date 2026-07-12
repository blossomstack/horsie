
/**
 * One repository visible to the App installation.
 */
export interface GitHubRepo {
  /**
   * &quot;owner/name&quot;.
   */
  fullName: string;
  private: boolean;
  defaultBranch: string;
}