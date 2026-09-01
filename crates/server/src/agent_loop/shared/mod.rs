//! What more than one component needs, and no one of them owns.
//!
//! The test for belonging here is *two callers in different components*, not
//! "it is a utility": a helper with one caller belongs in that component's own
//! module, where its reason for existing is visible.
//!
//! [`repair`] patches a history whose tool calls were left dangling — the turn
//! does it on recovery and on cancel, compaction before it summarises.
//! [`summarise`] is the bare summarising step and its budget — spent by
//! compaction for itself and by seeding for a sub session.
//! [`carried_state`] renders what must survive a compaction verbatim, read by
//! compaction and by seeding. [`agent_log`] pages and searches a transcript,
//! for the read component and the HTTP layer above it. [`hook_translation`]
//! decides what a hook record shows the model. [`workspace`] scans a runtime
//! and composes a system prompt, and [`mcp_toolbox`] composes the tools —
//! both for provisioning.

pub mod agent_log;
pub mod carried_state;
pub mod hook_translation;
pub mod mcp_toolbox;
pub mod repair;
pub mod summarise;
pub mod workspace;
