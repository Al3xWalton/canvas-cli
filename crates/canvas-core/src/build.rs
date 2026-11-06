//! What this build is: its version, git commit and source folder.
//!
//! Stamped at compile time by `build.rs`. Every binary carries it, so an
//! agent can compare the installed build against the newest on GitHub.

/// The crate version, e.g. `0.1.0`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Full git commit hash this build came from; empty if built outside a
/// git checkout.
pub const COMMIT: &str = env!("CANVAS_COMMIT");

/// Commit date as `YYYY-MM-DD`; empty if unknown.
pub const COMMIT_DATE: &str = env!("CANVAS_COMMIT_DATE");

/// True if the source had uncommitted changes when built.
pub const DIRTY: bool = !env!("CANVAS_DIRTY").is_empty();

/// The folder the source was built from (the git clone), used by
/// `canvas update` to pull and rebuild.
pub const SOURCE_DIR: &str = env!("CANVAS_SOURCE_DIR");

/// The public repository updates come from.
pub const REPO: &str = "Al3xWalton/canvas-cli";

/// [`version_line`], computed once, for `--version` flags.
pub static VERSION_LINE: std::sync::LazyLock<String> = std::sync::LazyLock::new(version_line);

/// One-line version, e.g. `0.1.0 (1a2b3c4 2026-11-06)`.
pub fn version_line() -> String {
    if COMMIT.is_empty() {
        return VERSION.to_owned();
    }
    let short = COMMIT.get(..7).unwrap_or(COMMIT);
    let dirty = if DIRTY { ", modified" } else { "" };
    format!("{VERSION} ({short} {COMMIT_DATE}{dirty})")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_line_names_the_version() {
        assert!(version_line().starts_with(VERSION));
    }
}
