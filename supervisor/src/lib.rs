//! Transport-agnostic supervision of parallel workflow jobs, built on the
//! event-sourced actor runtime. Shared by the CLI daemon and a future server mode.
//!
//! - [`SupervisorActor`] is the singleton job registry: it owns one [`JobActor`]
//!   per live job and rebuilds its registry by replaying its own journal.
//! - [`JobActor`] manages one job's lifecycle and resources (the sandboxed runtime
//!   child, the live log broadcast), delegating orchestration to the workflow crate.
//! - [`JobRuntime`] abstracts the executor/runtime assembly so the supervisor can
//!   be driven in tests without a real sandbox; [`ProcessJobRuntime`] is the
//!   production implementation.

mod history;
mod job_actor;
mod spec;
mod supervisor_actor;

pub use history::render_history;
pub use job_actor::{
    JobActor, JobCommand, JobDomainEvent, JobRuntime, JobShutdown, JobState, Kickoff, LaunchParams,
    LaunchedJob, ProcessJobRuntime, render_event,
};
pub use spec::{JobId, JobSpec, SupervisorDeps};
pub use supervisor_actor::{
    JobRecord, SupervisorActor, SupervisorCommand, SupervisorEvent, SupervisorState,
};
