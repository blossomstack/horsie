//! Named agent presets: a saved session configuration invoked with a message
//! to create a session. Mirrors the `memory` module's store/service split and
//! shares the config store's SqlitePool. Row types are hand-written storage
//! types; the fluorite wire types in `horsie_models::agents` are mapped at the
//! service boundary.

mod service;
mod store;

pub use service::{AgentError, AgentService};
pub use store::{AgentRow, AgentStore};
