
/**
 * Runtime preparation progress: `acquiring_runtime`, `scanning_workspace`,
 * `connecting_tools`, `ready`.
 */
export interface ProvisioningLifecycle {
  stage: string;
  detail?: string;
}