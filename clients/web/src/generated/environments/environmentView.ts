
import { EnvVar } from '../executor';
import { ProvisionStep } from '../executor';
import { RepoConfig } from '../session_api';
/**
 * An environment as shown to clients.
 */
export interface EnvironmentView {
  /**
   * Slug; the id of record, used in API paths.
   */
  name: string;
  description: string;
  /**
   * Runtime vendor name. Required, and never "local": environments only
   * target vendor-managed, provisionable runtimes.
   */
  vendor: string;
  /**
   * Repositories cloned into the runtime workspace at provision time.
   */
  repos: RepoConfig[];
  /**
   * Plain-text, non-sensitive env vars for the runtime. Secrets are a
   * future, separate concept.
   */
  envVars: EnvVar[];
  /**
   * Setup steps the runtime executes before its message loop. Inert today:
   * nothing provisions from an environment yet.
   */
  provision: ProvisionStep[];
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}