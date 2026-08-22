//! Plugins authored on this server: their rows, and the package rendered from
//! them.
//!
//! The database is the source of truth and the package is produced on demand,
//! so nothing on disk can be stale. See [`service`] for why the generation and
//! the digest are separate ideas.

pub mod render;
mod service;
mod store;
pub mod toolbox;

pub use service::{AuthoredService, pack};
pub use store::{AuthoredFile, AuthoredPluginRow, AuthoredSkillRow, AuthoredStore};
pub use toolbox::AuthoringToolbox;
