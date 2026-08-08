
import { EnvVar } from '../executor';
import { ProvisionStep } from '../executor';
import { RepoConfig } from '../session_api';
/**
 * Create or fully replace an environment. Omitted list fields default to
 * empty; `description` defaults to "".
 */
export interface EnvironmentInput {
  name: string;
  description?: string;
  vendor: string;
  repos?: RepoConfig[];
  envVars?: EnvVar[];
  provision?: ProvisionStep[];
}