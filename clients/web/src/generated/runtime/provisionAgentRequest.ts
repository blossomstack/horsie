
import { BundleRef } from './bundleRef';
/**
 * Install this agent's bundle set and make it the tree its tool calls, hooks,
 * scans and MCP servers read.
 *
 * Per agent, because an agent is the unit that has a plugin set: a workflow
 * step runs under its own preset, and that preset names its own bundles. The
 * session used to be the unit, baked into the runtime's environment at create
 * time, so every agent in a session got whatever the session was created with
 * and a step could not differ from its siblings at all.
 *
 * Idempotent, like `ProvisionWorkspace` and for the same reason: the server
 * sends it on every agent load rather than tracking what it already did. A
 * bundle already in the store is linked, not refetched, so the common case —
 * the same agent waking again — costs no I/O.
 *
 * An agent with no bundles is still provisioned, with an empty set. "Not
 * provisioned" is then a real state the runtime can refuse work in, rather than
 * something inferred from a directory that happens not to exist.
 */
export interface ProvisionAgentRequest {
  callId: string;
  agentId: string;
  bundles: BundleRef[];
}