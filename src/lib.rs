//! Compatibility-first Rust implementation of agent86.
//!
//! The crate contains the provider-neutral vocabulary and the model provider
//! layer. Orchestration, tools, persistence, and user interfaces will be added
//! behind explicit compatibility tests as the port progresses.

pub mod provider;
pub mod secrets;
pub mod types;
