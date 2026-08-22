
/**
 * One kind of thing an agent's log holds, as a reader chooses between them.
 *
 * A projection of [`AgentLogBody`] with its `Llm` arm split by role, because
 * that split is the distinction a reader actually makes: "what the person
 * said" and "what the model answered" are two questions, and both are `Llm`.
 *
 * Thinking is deliberately not a kind here. It is a *part* inside an assistant
 * message and never an entry of its own, so a `Thinking` arm would select
 * nothing and quietly return an empty page to anyone who asked for it. Whether
 * to keep thinking is the filter's other axis.
 */
export enum LogEntryKind {
  UserMessage = "UserMessage",
  AssistantMessage = "AssistantMessage",
  ToolResult = "ToolResult",
  Hook = "Hook",
  Lifecycle = "Lifecycle",
  Compaction = "Compaction",
}