
import { ToolError } from './toolError';
import { ToolOutput } from './toolOutput';
export type ToolResult =
  | { status: "Ok"; value: ToolOutput }
  | { status: "Err"; value: ToolError };