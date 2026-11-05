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
    /// The Canvas and ReCap addresses have not been set yet.
    #[error(
        "Canvas CLI is not set up yet. Run `canvas setup` to enter your Canvas and ReCap addresses."
    )]
    NotConfigured,
    /// No session is stored, or Canvas rejected the stored one.
    ///
    /// The message is the instruction a person or agent must follow: both
    /// the CLI and the MCP server surface it unchanged.
    #[error("Canvas session expired or missing. Run `canvas auth login`.")]
    SessionExpired,
    /// The OS keychain refused a read or write.
    #[error("keychain error: {0}")]
    Keychain(String),
    /// Driving Chrome for sign-in failed.
    #[error("browser sign-in failed: {0}")]
    Browser(String),
    /// A request to Canvas or Panopto failed for a reason other than auth.
    #[error("request failed: {0}")]
    Http(String),
    /// Canvas answered 404: the item does not exist or is hidden from the
    /// user.
    #[error("not found: {0}")]
    NotFound(String),
    /// Canvas answered 403: the user may not see this (e.g. a hidden tab).
    /// Distinct from [`Error::SessionExpired`], which is a 401.
    #[error("not allowed: {0}")]
    Forbidden(String),
    /// A local file could not be written or read.
    #[error("file error at {path}: {source}")]
    Io {
        /// Path involved.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The user's input could not be used, e.g. an ambiguous course name.
    #[error("{0}")]
    Input(String),
    /// A response did not have the shape we expected.
    #[error("unexpected response from {what}: {detail}")]
    Decode {
        /// Which endpoint answered.
        what: String,
        /// What was wrong with it.
        detail: String,
    },
}

/// Shorthand for results that fail with [`Error`].
pub type Result<T> = std::result::Result<T, Error>;
