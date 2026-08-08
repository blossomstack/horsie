
import { AgentSettings } from '../session';
import { RepoConfig } from './repoConfig';
/**
 * A session is created *with* the first thing to say to it. There is no
 * create-then-message shape: a session with no message is a provisioned
 * runtime nobody asked a question, and nothing reclaims one.
 */
export interface CreateSessionRequest {
  name?: string;
  agent: AgentSettings;
  /**
   * The first user message, queued as part of the create. Required and
   * non-empty.
   */
  message: string;
  /**
   * Runtime vendor name; defaults to "local".
   */
  vendor?: string;
  /**
   * Repositories cloned into a vendor-managed workspace at provision time.
   * Only honored by a vendor that supports provisioning; the UI sends these
   * only for such a vendor.
   */
  repos?: RepoConfig[];
  /**
   * Selected plugin-bundle names to provision for this session; absent →
   * the server's default-enabled bundles. Non-empty implies plugins are on.
   */
  plugins?: string[];
}