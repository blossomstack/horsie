
import { McpServerView } from './mcpServerView';
import { McpToolInfo } from './mcpToolInfo';
/**
 * One server *with* its remembered tools: what `GET /mcp/servers/{name}`
 * answers, and the only shape that carries tool descriptions.
 *
 * `tools` absent means the server has never successfully connected; an empty
 * list means it connected and advertised nothing.
 */
export interface McpServerDetail {
  server: McpServerView;
  tools?: McpToolInfo[];
}