# YAML Frontmatter Compatibility Design

## Goal

Parse Claude-compatible skill, agent, and command frontmatter as YAML so valid multiline and structured fields do not hide otherwise usable definitions.

## Design

Keep the existing `---` fence splitter in `support/src/frontmatter.rs`, but replace its flat line parser with YAML deserialization into a generic mapping. Consumers continue to read only the scalar fields they need; unknown fields, including `allowed-tools`, are ignored. This preserves the current narrow consumer APIs while accepting third-party provider metadata.

Use the workspace's Serde conventions and a YAML crate dependency. Malformed YAML remains a parse failure (`None` at the frontmatter helper boundary), preserving best-effort catalogue behavior. Add tests for Impeccable-style multiline lists, scalar extraction, unknown structured fields, and malformed YAML.

## Scope

Only frontmatter parsing and its dependency/tests change. No changes to catalogue semantics, runtime provisioning, or API wire types.
