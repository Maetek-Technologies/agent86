//! Compatibility-first Rust implementation of agent86.
//!
//! The crate contains the provider-neutral vocabulary, layered configuration,
//! the model provider layer, and a text-only one-shot turn. Orchestration,
//! tools, persistence, and user interfaces will be added behind explicit
//! compatibility tests as the port progresses.

pub mod agent;
pub mod approval;
pub mod config;
pub mod config_writer;
pub mod difflib;
pub mod guardrails;
pub mod history;
pub mod keys;
pub mod mcp;
pub mod memory;
pub mod mentions;
pub mod observability;
pub mod pricing;
pub mod prompt;
pub mod provider;
pub mod pyjson;
pub mod repl;
pub mod secrets;
pub mod session;
pub mod setup;
pub mod tools;
pub mod turn;
pub mod types;
pub mod ui;
