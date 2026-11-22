//! agent86UI's protocol layer: Google's [A2UI](https://a2ui.org) 1.0 for Rust.
//!
//! A2UI lets an agent describe UI as declarative JSON that a renderer draws with
//! its own widgets, from a catalog of components and functions it trusts. This
//! crate is the agent's side of it, with no dependency on the rest of agent86:
//!
//! - [`Catalog`]: a catalog loaded and checked against the specification's
//!   rules, and [`Catalog::basic`], the specification's basic catalog;
//! - [`AgentMessage`] and [`RendererMessage`]: the messages, typed;
//! - [`Validator`]: checks what an agent's model wrote, batch by batch, against
//!   the schemas and the rules they cannot express (surfaces, `root`, child
//!   references, composition, function boundaries), with the specification's
//!   error codes and paths, so the problems can go back to the model;
//! - [`runtime`]: the headless surface runtime a renderer builds on: surfaces'
//!   components and data models, every binding, call, template, and check
//!   resolved for drawing ([`runtime::Runtime::tree`]), user input written back,
//!   and actions dispatched, with the basic catalog's functions (formatting in
//!   `en-US`, the regex dialect and its safety rules) implemented;
//! - [`SchemaSet`]: the envelope schemas compiled against a catalog, for the
//!   schema half of validation alone;
//! - [`is_identifier`]: the specification's naming rule.
//!
//! The JSON Schemas and the basic catalog are vendored from the specification
//! (`spec/`, Apache-2.0, upstream commit recorded in `spec/UPSTREAM.md`) and
//! served from memory: nothing is ever fetched. The tests replay the
//! specification's schema cases and its shared conformance suites for the
//! runtime (`conformance/core/` upstream).
//!
//! agent86UI is agent86's implementation of A2UI; A2UI is Google's protocol, and
//! this crate is not affiliated with or endorsed by Google.

mod catalog;
mod ident;
mod message;
pub mod runtime;
mod schema;
mod validate;

pub use catalog::{
    Callers, Catalog, CatalogError, ChildRef, ComponentDef, FunctionDef,
    ALLOWED_EXTERNAL_REF_TARGETS, BASIC_CATALOG_ID,
};
pub use ident::is_identifier;
pub use message::{
    Action, AgentMessage, AgentMessageKind, CallAgentFunction, CallRendererFunction, CheckRule,
    Component, CreateSurface, DeleteSurface, ErrorBody, FunctionCall, FunctionResponse,
    RendererError, RendererMessage, RendererMessageKind, UpdateComponents, UpdateDataModel,
};
pub use schema::{
    check_agent_capabilities, check_catalog_definition, check_renderer_capabilities,
    check_renderer_data_model, CompileError, SchemaError, SchemaSet, SPEC_BASE,
};
pub use validate::{Code, Problem, Report, Rules, Validator, MAX_CALL_DEPTH, MAX_DEPTH};

/// The A2UI protocol version this crate implements, as messages carry it in
/// their `version` field.
pub const PROTOCOL_VERSION: &str = "1.0";

/// The README's example, compiled and run as a doctest so it cannot drift.
#[doc = include_str!("../README.md")]
#[cfg(doctest)]
pub struct ReadmeDoctests;
