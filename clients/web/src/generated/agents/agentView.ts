
/**
 * An agent preset as shown to clients.
 */
export interface AgentView {
  /**
   * Slug; the id of record, used in API paths and CLI invocations.
   */
  name: string;
  /**
   * What this preset is for, as shown in the roster. Never sent to the
   * model — `instructions` is what the model reads.
   */
  description: string;
  /**
   * Standing instructions this preset's agent runs under, added to the
   * system prompt as its own section. Absent → the agent behaves exactly
   * like an unpresetted one.
   */
  instructions?: string;
  /**
   * Configured model alias.
   */
  model: string;
  /**
   * Selected plugin-bundle (skill) names.
   */
  plugins: string[];
  /**
   * Enabled MCP server names.
   */
  mcpServers: string[];
  /**
   * Memory spaces the session may read and write.
   */
  memorySpaces: string[];
  /**
   * Canonical thinking effort; absent → the model's configured default.
   */
  thinkingEffort?: string;
  /**
   * Whether sessions from this preset compact automatically once their
   * context fills; absent → yes.
   */
  autoCompact?: boolean;
  /**
   * The tools sessions from this preset may call, by name. Absent → the
   * default set (every built-in group except the control plane), which is
   * what lets a preset saved today follow a later horsie's idea of sensible
   * rather than freezing this one's list.
   *
   * Only built-in tools are governed. Skills, MCP servers and memory spaces
   * are chosen by their own fields, so a narrowed selection never silently
   * removes what one of those turned on.
   */
  allowedTools?: string[];
  /**
   * Whether this preset opts in to being tuned from its own past runs: a
   * scheduled agent may read what sessions on this preset did, work out what
   * would make it better, and write the preset back.
   *
   * Absent → no. The opposite default to `auto_compact`, and deliberately:
   * compaction not happening costs a session its context, while tuning
   * happening unasked lets one agent rewrite another's instructions. Opting
   * in has to be an act.
   */
  tunable?: boolean;
  /**
   * Which version of this preset this is. Pass it back as
   * `expected_revision` on a write and the write is refused if anything
   * changed in between. Absent on a preset that predates versioning and has
   * not been written since.
   */
  revision?: number;
  /**
   * Unix epoch seconds.
   */
  createdAt: string;
  updatedAt: string;
}