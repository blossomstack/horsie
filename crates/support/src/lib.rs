//! Hand-written helpers shared by the horsie binaries — the counterpart to
//! `horsie-models`, which holds only generated wire types.
//!
//! Every item lives under a domain module; nothing is exported at the crate
//! root. A module that acquires its own heavy dependencies graduates into its
//! own crate.

pub mod dial_token;
pub mod frontmatter;
#[cfg(feature = "git")]
pub mod git;
pub mod mcp;
pub mod plugin;
pub mod remote_url;
pub mod tls;
