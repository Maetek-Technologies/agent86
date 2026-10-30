//! agent86, an agentic harness on the command line.
//!
//! It began as a compatibility-first rewrite of the Python implementation, and
//! keeps the formats the two share (config, sessions, memory, traces, keyring)
//! compatible with it.

pub mod agent;
pub mod approval;
pub mod config;
pub mod config_writer;
pub mod difflib;
pub mod guardrails;
pub mod history;
pub mod keys;
pub mod limits;
pub mod mcp;
pub mod memory;
pub mod mentions;
pub mod observability;
pub mod otel;
pub mod pricing;
pub mod prompt;
pub mod provider;
pub mod pyjson;
pub mod repl;
pub mod router;
pub mod secrets;
pub mod session;
pub mod setup;
pub mod skills;
pub mod subagent;
pub mod tools;
pub mod turn;
pub mod types;
pub mod ui;
pub mod vault;
