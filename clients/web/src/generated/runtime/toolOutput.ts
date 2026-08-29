
export interface ToolOutput {
  stdout: string;
  stderr: string;
  exitCode: number;
  /**
   * Bytes the call produced that are not text — an MCP screenshot block.
   * Bytes rather than references because the runtime has no database: it
   * ships what it got and the server stores it, so what a message ends up
   * holding is an `ArtifactRef` and never this.
   *
   * Defaulted, and that is load-bearing rather than tidy: this type crosses
   * a version boundary between a runtime process and the server they connect
   * to. A required field here would fail to deserialize for every runtime
   * binary built before it existed.
   */
  artifacts: string[];
}