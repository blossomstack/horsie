
import { ArtifactRef } from './artifactRef';
/**
 * The result of executing a tool call
 */
export interface ToolResultPart {
  toolCallId: string;
  output: string;
  isError: boolean;
  /**
   * What the call produced that is not text — a screenshot, a generated
   * PDF. A field beside `output` rather than a separate part, because a
   * tool result is one thing that has both a transcript and artifacts.
   *
   * Defaulted, and that is load-bearing rather than tidy: this type is
   * journaled, so every tool result written before this field existed
   * deserializes without it. A required field here would fail `recover()`
   * for every session that has ever called a tool.
   */
  artifacts: ArtifactRef[];
}