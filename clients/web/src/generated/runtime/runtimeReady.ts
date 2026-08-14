
/**
 * First message the runtime sends after connecting, and the only one it sends
 * unprompted.
 *
 * Asserts one thing: the process is up, confined, and listening. It used to
 * assert three — that provisioning had also been attempted and had also
 * succeeded — which is why a session whose checkout silently failed was
 * indistinguishable from one that never had a checkout to do.
 */
export interface RuntimeReady {
  runtimeId: string;
}