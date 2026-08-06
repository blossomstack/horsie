
/**
 * Runtime preparation progress: `acquiring_runtime`, `scanning_workspace`,
 */
export interface ProvisioningLifecycle {
  stage: string;
  detail?: string;
}