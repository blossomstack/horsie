//! Named environments (experimental): a reusable runtime + repos bundle.
//! Mirrors the `agents` module's store/service split. Row types are
//! hand-written storage types; the fluorite wire types in
//! `horsie_models::environments` are mapped at the service boundary.

mod service;
mod store;

pub use service::{EnvironmentError, EnvironmentService};
pub use store::{EnvironmentRow, EnvironmentStore};
