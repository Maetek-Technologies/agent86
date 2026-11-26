//! The agent behind agent86: a safe agent runtime you can embed.
//!
//! [`sdk`] is the embedding API, and the only module with a stability promise:
//! [`sdk::Agent::builder`] assembles the harness from parts the caller passes in,
//! with the workspace jail, the approval gate, the guardrails, the sandbox, and
//! the secret vault's redaction already in place.
//!
//! Every other module is public because the `agent86` binary (the terminal
//! interface, the REPL, `run`, and the ACP server) is built on them. They carry no
//! stability promise and are hidden from these docs; the binary builds this crate
//! from the same checkout.

pub mod sdk;

/// Compiles the README's example (shown on crates.io) as a doctest, so it
/// cannot drift from the API.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
pub struct ReadmeDoctests;

#[doc(hidden)]
pub mod agent;
#[cfg(feature = "agent86ui")]
#[doc(hidden)]
pub mod agent86ui;
#[doc(hidden)]
pub mod agents_def;
#[doc(hidden)]
pub mod approval;
#[doc(hidden)]
pub mod ask;
#[doc(hidden)]
pub mod broken_json;
#[doc(hidden)]
pub mod config;
#[doc(hidden)]
pub mod config_writer;
#[doc(hidden)]
pub mod difflib;
#[doc(hidden)]
pub mod escape;
#[doc(hidden)]
pub mod guardrails;
#[doc(hidden)]
pub mod limits;
#[doc(hidden)]
pub mod mcp;
#[doc(hidden)]
pub mod memory;
#[doc(hidden)]
pub mod observability;
#[doc(hidden)]
pub mod otel;
#[doc(hidden)]
pub mod pricing;
#[doc(hidden)]
pub mod prompt;
#[doc(hidden)]
pub mod provider;
#[doc(hidden)]
pub mod pyjson;
#[doc(hidden)]
pub mod router;
#[doc(hidden)]
pub mod secrets;
#[doc(hidden)]
pub mod session;
#[doc(hidden)]
pub mod skills;
#[doc(hidden)]
pub mod specialty_turn;
#[doc(hidden)]
pub mod subagent;
#[doc(hidden)]
pub mod tools;
#[doc(hidden)]
pub mod trust;
#[doc(hidden)]
pub mod turn;
#[doc(hidden)]
pub mod types;
#[doc(hidden)]
pub mod vault;
#[doc(hidden)]
pub mod verify;
#[doc(hidden)]
pub mod worktree;
