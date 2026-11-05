//! The error type shared across the workspace.
//!
//! Library crates return [`Error`] so both front ends (CLI and MCP) can
//! map failures to the same messages. Binaries wrap it in `anyhow` only
//! at their edge.

use std::path::PathBuf;

/// A failure anywhere in Canvas CLI.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The config file exists but could not be read.
    #[error("could not read config at {path}: {source}")]
    ConfigRead {
        /// Path of the config file.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The config file is not valid TOML for [`crate::Config`].
    #[error("invalid config at {path}: {source}")]
    ConfigParse {
        /// Path of the config file.
        path: PathBuf,
        /// Underlying parse error.
        source: Box<toml::de::Error>,
    },
    /// No home or config directory could be found for this user.
    #[error("could not find a config directory for this user")]
    NoConfigDir,
}

/// Shorthand for results that fail with [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
