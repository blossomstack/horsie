
import { RuntimeSpec } from './runtimeSpec';
/**
 * Hand back an existing runtime, resuming it if it was hibernated.
 *
 * Carries the spec because the server is the only durable holder of it. A
 * vendor that wrote its own copy down would be a second owner of one fact,
 * and the two would drift the moment a session's environment changed.
 *
 * Carrying it is *not* permission to provision from nothing. A vendor with no
 * runtime under this id still fails the request, and the server turns that
 * into a terminally unrecoverable session rather than silently rebuilding a
 * workspace the user believes still exists. The spec is how to rebuild a
 * runtime the vendor knows it owns — never how to invent one.
 *
 * A get issued while a create for the same id is in flight must wait for that
 * create to resolve, and concurrent gets must never yield two runtimes.
 */
export interface GetRuntimeRequest {
  runtimeId: string;
  spec: RuntimeSpec;
}