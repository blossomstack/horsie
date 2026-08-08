
import { EnvVar } from '../executor';
import { ProvisionStep } from '../executor';
import { RepoConfig } from '../session_api';
/**
 * A predefined environment as shown to clients.
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
   * future, separate concept. Names in the server's reserved `HORSIE_*`
   * namespace are refused at save.
   */
  envVars: EnvVar[];
  /**
   * Setup steps the runtime executes before its message loop, after the
   * repo checkouts — a step like `make setup` needs its repo on disk.
   */
  provision: ProvisionStep[];
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}