//! Workflows: a named graph of steps, each step an agent preset plus a fixed
//! prompt, wired by conditions over the step's structured output.
//!
//! The store/service split mirrors `agents` and `routines`, which a workflow
//! references rather than duplicates: a definition answers "which agents, in
//! what order, on what condition?", never "how is this agent configured?".
//!
//! A *run* of a workflow is not here — it is a session, driven by
//! `sessions::workflow`.

mod service;
mod store;

pub use service::{WorkflowError, WorkflowService, step_named};
pub use store::{WorkflowRow, WorkflowStore};
