//! The agent-run index: which agents ran under which preset, and how they
//! ended.
//!
//! A read model, not a source of truth. Everything here is derived from a
//! session actor's own state and can be rebuilt from it; the table exists
//! because that state is spread across one journal per session and no journal
//! can be asked "which of you ran preset P".
//!
//! Store only, no service. The two callers want different things and neither
//! wants a layer between: the session actor writes rows it derived itself, and
//! the control plane reads them straight back out. A service here would be a
//! pass-through with a second error type.

mod store;

pub use store::{AgentRunFilter, AgentRunRow, AgentRunStore};
