//! Routines: an agent preset plus a fixed prompt and a trigger.
//!
//! The store/service split mirrors `agents`, which routines reference rather
//! than duplicate — a routine answers "what should this agent do, and when?",
//! never "how is this agent configured?". Two pieces sit above the service: the
//! `RoutineRunner`, which every trigger path goes through, and the
//! `RoutineScheduler`, which is just a clock on top of the runner.

mod runner;
mod scheduler;
mod service;
mod store;

pub use runner::RoutineRunner;
pub use scheduler::{RoutineScheduler, TICK_INTERVAL};
pub use service::{MIN_INTERVAL_SECS, RoutineError, RoutineService, next_run_at};
pub use store::{RoutineRow, RoutineStore, RunOutcome, Schedule};
