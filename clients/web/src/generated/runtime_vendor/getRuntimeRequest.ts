
/**
 * Hand back an existing runtime, resuming it if it was hibernated.
 *
 * Deliberately carries no spec: a get must never be able to provision. A
 * vendor with no runtime under this id fails the request, and the server
 * turns that into a terminally unrecoverable session rather than silently
 * rebuilding a workspace the user believes still exists.
 *
 * A get issued while a create for the same id is in flight must wait for that
 * create to resolve, and concurrent gets must never yield two runtimes.
 */
export interface GetRuntimeRequest {
  runtimeId: string;
}