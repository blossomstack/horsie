
import { AuthoredOrigin } from './authoredOrigin';
import { ExternalOrigin } from './externalOrigin';
/**
 * What a bundle is: where its bytes come from, and therefore how it is
 * identified, updated and rendered.
 *
 * A union rather than a `source_kind` string beside four optional columns, so
 * "no URL" cannot mean two different things — the same strengthening
 * `IngestTarget` made on the ingest side. Refusing to re-clone something that
 * was never cloned is then a match arm rather than a guard someone has to
 * remember to write.
 */
export type PluginKind =
  /**
   * Claude Code's layout: `.claude-plugin/plugin.json`, `.mcp.json`, and
   * horsie's hooks, agents and commands conventions.
   */
  | { kind: "Claude"; value: ExternalOrigin }
  /**
   * Agent Plugins 1.0: a root `plugin.json` carrying a `$schema`, skills
   * fixed at `skills/`, MCP servers at `mcp.json`.
   */
  | { kind: "AgentPlugin"; value: ExternalOrigin }
  /**
   * Authored here. Rendered as Agent Plugins 1.0 when a runtime asks for it.
   */
  | { kind: "Authored"; value: AuthoredOrigin };