#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::wildcard_enum_match_arm
)]

mod server;
pub use server::{
    BlockHandle, MockLlmServer, MockLlmServerBuilder, MockResponse, Scenario, ScenarioConfig,
};
