
/**
 * Create an authored plugin, or change its description. `name` must satisfy
 * the Agent Plugins name grammar — it is rendered into a `plugin.json` that
 * any conformant client has to be able to read, not just this one.
 */
export interface AuthoredPluginWriteInput {
  name: string;
  description?: string;
}