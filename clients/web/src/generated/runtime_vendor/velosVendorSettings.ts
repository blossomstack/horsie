
/**
 * How a velos vendor schedules containers.
 */
export interface VelosVendorSettings {
  /**
   * The velos server root, e.g. `http://velos:8080`.
   */
  serverUrl: string;
  /**
   * OCI image bundling `horsie-runtime`, built without the sandbox feature —
   * the container is already the isolation boundary.
   */
  image: string;
  /**
   * Path to `horsie-runtime` inside the image.
   */
  runtimeBin: string;
  /**
   * Where in the container workspaces are allocated.
   */
  workspaceRoot: string;
  /**
   * The `ws://` URL a container reaches this server on, *from velos's
   * container network* — not necessarily the address a browser uses.
   * Includes the connect path, as on a Fly vendor.
   */
  callbackUrl: string;
  cpu: number;
  memoryMb: number;
}