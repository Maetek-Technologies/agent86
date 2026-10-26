//! A compatibility-first Rust implementation of agent86.
//!
//! The crate currently provides the provider-neutral vocabulary, layered
//! configuration for model providers, an OpenAI-compatible streaming provider,
//! and a single text-only turn. Tools, the ReAct loop, persistence, and the
//! interactive interface will be added behind explicit compatibility tests as
//! the port progresses.

pub mod config;
pub mod orchestration;
pub mod provider;
pub mod types;
