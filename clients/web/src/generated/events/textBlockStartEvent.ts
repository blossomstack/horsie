
/**
 * Streaming content blocks — emitted by the provider; index = content block
 * position. Every block follows the same lifecycle, keyed by index:
 * Start → Delta* → ContentBlockStop
 * Only the tool start carries identity (id+name); text/thinking starts are pure
 * boundary markers. ContentBlockStop is the one generic terminator for all kinds.
 */
export interface TextBlockStartEvent {
  messageId: string;
  index: number;
}