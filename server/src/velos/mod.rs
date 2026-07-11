//! A thin client for the [velos](https://github.com/blossomstack/velos) control
//! plane — just enough of its container REST API to schedule, observe, and
//! reclaim the remote sandboxes the [`crate::vendor::VelosVendor`] runs on.
//!
//! The [`ContainerApi`] trait is the seam the vendor depends on; [`VelosClient`]
//! is the real REST implementation, and tests substitute a double that spawns a
//! local reverse-dial runtime instead of a real micro-VM.

mod client;

pub use client::{ContainerApi, ContainerLaunchSpec, ContainerPhase, VelosClient, VelosError};
