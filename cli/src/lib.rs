//! The `october` CLI: load a workflow + JSON config, run it against a workdir with
//! all tool execution confined in the nono sandbox, and suspend/resume on
//! `ask_user`. The first end-to-end wiring of the full stack in production code.

pub mod config;
pub mod error;
pub mod run;
pub mod terminal_sink;
pub mod validate;
