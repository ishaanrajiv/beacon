//! Beacon's shared types. Everything here is plain data and pure functions: the store, the server,
//! codegen and the command all build on it, and none of it opens a socket or a database.
//!
//! - [`model`]: classes, push levels, item states, push statuses and reasons, actions, icons.
//! - [`decl`]: a source's `kinds.toml`, parsed and checked (`spec/declarations.md`).
//! - [`template`]: `{placeholder}` templates, value cleaning and word-boundary truncation.
//! - [`render`]: an event's facts and slots checked against its kind and rendered to text.
//! - [`builtin`]: the kinds Beacon owns (`beacon.*`, `trail.*`).
//! - [`ids`]: the collapse ids pushes carry.
//! - [`duration`], [`names`], [`problem`]: the small pieces the rest share.

pub mod builtin;
pub mod decl;
pub mod duration;
pub mod ids;
pub mod model;
pub mod names;
pub mod problem;
pub mod render;
pub mod template;

pub use problem::Problem;
