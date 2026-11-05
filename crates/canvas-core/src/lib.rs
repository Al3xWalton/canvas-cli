//! Shared foundations for every Canvas CLI crate.
//!
//! `canvas-core` owns what the other crates agree on: the user's
//! [`Config`] (which Canvas site and which Panopto host to talk to, and
//! where downloads go) and the shared [`Error`] type. It does no network
//! I/O and holds no secrets. Credentials live in `canvas-auth` and the
//! OS keychain, never in the config file.
//!
//! Neighbours: `canvas-auth`, `canvas-api`, `canvas-sync` and `panopto`
//! read a [`Config`]; `canvas-cli` and `canvas-mcp` load it at start-up.

mod config;
mod error;
mod names;

pub use config::Config;
pub use error::{Error, Result};
pub use names::sanitize;
