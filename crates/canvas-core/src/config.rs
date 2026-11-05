//! User configuration, loaded from `~/.config/canvas-cli/config.toml`.
//!
//! Every field has a default, so a missing file is not an error: a new
//! user at the university can run the tool with no setup. The file never holds
//! secrets.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Where Canvas CLI points and where it writes downloads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Base URL of the Canvas site, e.g. `https://canvas.example.edu`.
    pub canvas_url: String,
    /// Host of the Panopto ReCap tenant, e.g. `example.hosted.panopto.com`.
    pub panopto_host: String,
    /// Root directory for downloaded files and recordings.
    pub download_root: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            canvas_url: "https://canvas.example.edu".into(),
            panopto_host: "example.hosted.panopto.com".into(),
            download_root: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Canvas"),
        }
    }
}

impl Config {
    /// Returns the config file path: `$CANVAS_CLI_CONFIG` if set, otherwise
    /// `~/.config/canvas-cli/config.toml`.
    ///
    /// # Errors
    /// Returns [`Error::NoConfigDir`] if the user has no home directory.
    pub fn default_path() -> Result<PathBuf> {
        if let Some(path) = std::env::var_os("CANVAS_CLI_CONFIG") {
            return Ok(PathBuf::from(path));
        }
        dirs::home_dir()
            .map(|h| h.join(".config/canvas-cli/config.toml"))
            .ok_or(Error::NoConfigDir)
    }

    /// Loads the config from `path`, or the defaults if the file is absent.
    ///
    /// # Errors
    /// Returns [`Error::ConfigRead`] if the file exists but cannot be read,
    /// and [`Error::ConfigParse`] if it is not valid.
    ///
    /// # Example
    /// ```
    /// let config = canvas_core::Config::load(std::path::Path::new("/no/such/file")).unwrap();
    /// assert_eq!(config.panopto_host, "example.hosted.panopto.com");
    /// ```
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(text) => toml::from_str(&text).map_err(|source| Error::ConfigParse {
                path: path.to_owned(),
                source: Box::new(source),
            }),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(source) => Err(Error::ConfigRead {
                path: path.to_owned(),
                source,
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_other_defaults() {
        let dir = std::env::temp_dir().join(format!("canvas-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "canvas_url = \"https://example.instructure.com\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.canvas_url, "https://example.instructure.com");
        assert_eq!(config.panopto_host, "example.hosted.panopto.com");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn unknown_field_is_refused() {
        let err = toml::from_str::<Config>("token = \"x\"\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }
}
