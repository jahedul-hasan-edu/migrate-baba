//! Migrate Baba core engine: connections, catalog introspection, schema diff,
//! migration plan generation, deployment and the parallel data engine.
//!
//! The crate is UI-agnostic: both the desktop app (`mb-app`) and the CLI (`mb-cli`) use it.

pub mod conn;
pub mod data;
pub mod ddl;
pub mod deploy;
pub mod diff;
pub mod error;
pub mod ident;
pub mod introspect;
pub mod model;
pub mod plan;
pub mod progress;

pub use error::{Error, Result};
