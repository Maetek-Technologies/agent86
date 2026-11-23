//! agent86, an agentic harness on the command line.
//!
//! It began as a compatibility-first rewrite of the Python implementation, and
//! keeps the formats the two share (config, sessions, memory, traces, keyring)
//! compatible with it.
//!
//! This crate is the binary's surfaces: the terminal interface, the REPL, `run`,
//! the ACP server, the CLI commands, and the assembly from config (`setup`). The
//! agent itself is `agent86-core`, whose modules are re-exported here under
//! their own names, so `crate::agent`, `agent86::tools`, and the rest resolve as
//! they did before the split.

pub mod acp;
pub mod ask_text;
pub mod auth;
pub mod history;
pub mod keys;
pub mod mentions;
pub mod repl;
pub mod session_dirs;
pub mod setup;
pub mod ui;
pub mod update;

pub use agent86_core::{
    agent, agent86ui, agents_def, approval, ask, broken_json, config, config_writer, difflib,
    escape, guardrails, limits, mcp, memory, observability, otel, pricing, prompt, provider,
    pyjson, router, sdk, secrets, session, skills, specialty_turn, subagent, tools, trust, turn,
    types, vault, worktree,
};
