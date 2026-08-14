
/**
 * Connect to every MCP server the loaded plugins declare and list their tools.
 *
 * One request rather than one per server: a session wants the whole tool list
 * or none of it, and a server that cannot start contributes nothing rather than
 * failing the scan.
 * `agent_id` names whose bundles declare the servers. Two agents with different
 * bundles declare different servers, so the runtime's registry is per agent
 * rather than per connection.
 */
export interface McpDiscoverRequest {
  callId: string;
  agentId: string;
}