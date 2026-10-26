//! Compatibility-first Rust implementation of agent86.
//!
//! The crate contains the provider-neutral vocabulary, layered configuration,
//! the model provider layer, and a text-only one-shot turn. Orchestration,
//! tools, persistence, and user interfaces will be added behind explicit
//! compatibility tests as the port progresses.

pub mod config;
pub mod pricing;
pub mod prompt;
pub mod provider;
pub mod pyjson;
pub mod secrets;
pub mod turn;
pub mod types;
