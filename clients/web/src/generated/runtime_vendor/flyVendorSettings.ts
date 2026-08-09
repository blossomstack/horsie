
/**
 * How a Fly Machines vendor builds machines.
 */
export interface FlyVendorSettings {
  /**
   * The Fly app machines are created in. Must already exist — the server
   * creates machines, not apps.
   */
  app: string;
  /**
   * OCI image with `horsie-runtime` baked in.
   */
  image: string;
  region: string;
  /**
   * Where in the machine workspaces are allocated.
   */
  workspaceRoot: string;
  /**
   * The `ws://`/`wss://` URL a machine reaches this server on, including the
   * connect path — a URL with no path is refused rather than completed, so
   * what is stored is always what was sent. An address that only resolves on
   * the server's own loopback is refused too: a machine could never reach
   * it, and the failure would otherwise surface as an unexplained session
   * timeout.
   */
  callbackUrl: string;
  /**
   * Give each runtime a volume, so a stopped one keeps its workspace.
   */
  volumes: boolean;
  cpuKind: string;
  cpus: number;
  memoryMb: number;
  volumeSizeGb: number;
}