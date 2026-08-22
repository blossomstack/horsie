
import { NamedEnvironment } from './namedEnvironment';
import { NoRuntime } from './noRuntime';
import { RuntimeEnvironment } from './runtimeEnvironment';
/**
 * Where a session runs and what it runs against. Required by every path that
 * creates one: an optional environment would be a second, invisible way to
 * answer the question, settled by a server default nobody asked for.
 */
export type EnvironmentSpec =
  | { type: "Runtime"; value: RuntimeEnvironment }
  | { type: "Named"; value: NamedEnvironment }
  | { type: "None"; value: NoRuntime };