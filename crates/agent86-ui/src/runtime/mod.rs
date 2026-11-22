//! The headless surface runtime: what a renderer does, without drawing.
//!
//! A surface is its components and its data model. The runtime applies the
//! agent's messages to them, resolves each component's dynamic values
//! (bindings, function calls, templates), evaluates its checks, writes user
//! input back, and builds the `action` a user's gesture sends. A renderer (the
//! terminal interface, a text fallback, a test) draws from the resolved values;
//! one that draws itself (a React app with Google's renderer) does not need it.
//!
//! Behavior follows the specification and its shared conformance suites
//! (`conformance/core/` upstream), which `tests/runtime_conformance.rs` replays.

pub mod data;
pub mod expr;
pub mod format;
pub mod functions;
pub mod regex;
pub mod surface;

pub use data::{DataError, DataModel, WatchId, MAX_ARRAY_INDEX};
pub use expr::{ExpressionError, Part};
pub use functions::{coerce_bool, Effect, EvalError, Evaluator, LocalFunction, Scope};
pub use surface::{ActionError, Dispatch, Gesture, Node, Runtime};
