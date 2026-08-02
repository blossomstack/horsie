// Single import surface for the fluorite-generated protocol types. Regenerate
// with `bun run generate-types` whenever the horsie `.fl` schemas change.
export * from "../generated/agent";
export * from "../generated/auth";
export * from "../generated/capabilities";
export * from "../generated/github";
export * from "../generated/mcp";
export * from "../generated/memory";
export * from "../generated/model_cards";
export * from "../generated/plugins";
export * from "../generated/session";
export * from "../generated/session_api";
export * from "../generated/settings";

// Agent presets. Explicit rather than `export *`: the agent-loop package
// (`agent.fl`) also defines an `AgentInput`, and the flat re-export surface can
// hold only one — app code means the preset.
export type {
  AgentInput,
  AgentInvokeRequest,
  AgentInvokeResponse,
  AgentView,
} from "../generated/agents";
