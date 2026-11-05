//! User configuration, loaded from `~/.config/canvas-cli/config.toml`.
//!
//! The Canvas and ReCap addresses have **no defaults**: every university
//! has its own, and `canvas setup` asks for them (see [`canvas_origin`] and
//! [`recap_host`]). A missing file loads as unconfigured, and commands that
//! need the addresses call [`Config::require`], which explains what to do.
//! The file never holds secrets.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Where Canvas CLI points and where it writes downloads.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Base URL of the Canvas site, e.g. `https://canvas.example.edu`.
    /// Empty until `canvas setup` runs.
    pub canvas_url: String,
    /// Host of the Panopto ReCap site, e.g. `example.hosted.panopto.com`.
    /// Empty until `canvas setup` runs.
    pub panopto_host: String,
    /// Root directory for downloaded files and recordings.
    pub download_root: PathBuf,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            canvas_url: String::new(),
            panopto_host: String::new(),
            download_root: dirs::home_dir()
                .unwrap_or_else(|| PathBuf::from("."))
                .join("Canvas"),
        }
    }
}

impl Config {
    /// Returns `self` if both addresses are set.
    ///
    /// # Errors
    /// [`Error::NotConfigured`] if either is missing.
    pub fn require(self) -> Result<Self> {
        if self.canvas_url.trim().is_empty() || self.panopto_host.trim().is_empty() {
            Err(Error::NotConfigured)
        } else {
            Ok(self)
        }
    }

    /// True if both addresses are set.
    pub fn is_configured(&self) -> bool {
        !self.canvas_url.trim().is_empty() && !self.panopto_host.trim().is_empty()
    }

    /// Writes the config to `path`, creating its folder.
    ///
    /// # Errors
    /// [`Error::ConfigRead`] (with the write error) if it cannot be saved.
    pub fn save(&self, path: &Path) -> Result<()> {
        let io = |source| Error::ConfigRead {
            path: path.to_owned(),
            source,
        };
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(io)?;
        }
        let text = toml::to_string(self).map_err(|e| Error::ConfigRead {
            path: path.to_owned(),
            source: std::io::Error::other(e.to_string()),
        })?;
        std::fs::write(path, text).map_err(io)
    }

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
    /// assert!(!config.is_configured());
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

/// Turns what a user types for their Canvas site into its https origin:
/// `canvas.example.edu`, `https://canvas.example.edu/courses/1` and
/// `https://canvas.example.edu/` all give `https://canvas.example.edu`.
///
/// # Errors
/// [`Error::Input`] if it is not a web address, or uses plain http.
///
/// # Example
/// ```
/// assert_eq!(canvas_core::canvas_origin("canvas.example.edu/courses/5").unwrap(), "https://canvas.example.edu");
/// assert!(canvas_core::canvas_origin("http://canvas.example.edu").is_err());
/// ```
pub fn canvas_origin(text: &str) -> Result<String> {
    let url = parse_web(text)?;
    if url.scheme() != "https" {
        return Err(Error::Input("the Canvas address must use https".into()));
    }
    let host = url.host_str().ok_or_else(|| bad_address(text))?;
    Ok(match url.port() {
        Some(port) => format!("https://{host}:{port}"),
        None => format!("https://{host}"),
    })
}

/// Turns what a user types for ReCap (Panopto) into its host name. A bare
/// host or any ReCap link works, e.g. a lecture's viewer link:
/// `https://example.hosted.panopto.com/Panopto/Pages/Viewer.aspx?id=...`
/// gives `example.hosted.panopto.com`.
///
/// # Errors
/// [`Error::Input`] if no host name can be found.
///
/// # Example
/// ```
/// let host = canvas_core::recap_host("https://example.hosted.panopto.com/Panopto/Pages/Viewer.aspx?id=1").unwrap();
/// assert_eq!(host, "example.hosted.panopto.com");
/// assert_eq!(canvas_core::recap_host("example.hosted.panopto.com").unwrap(), "example.hosted.panopto.com");
/// ```
pub fn recap_host(text: &str) -> Result<String> {
    let url = parse_web(text)?;
    let host = url
        .host_str()
        .ok_or_else(|| bad_address(text))?
        .to_ascii_lowercase();
    if !host.contains('.') {
        return Err(bad_address(text));
    }
    Ok(host)
}

fn parse_web(text: &str) -> Result<url::Url> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Input("an address is needed".into()));
    }
    let with_scheme = if text.contains("://") {
        text.to_owned()
    } else {
        format!("https://{text}")
    };
    let url = url::Url::parse(&with_scheme).map_err(|_| bad_address(text))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(bad_address(text));
    }
    Ok(url)
}

fn bad_address(text: &str) -> Error {
    Error::Input(format!("`{text}` is not a web address"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partial_file_keeps_other_defaults() {
        let dir = std::env::temp_dir().join(format!("canvas-cli-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(&path, "canvas_url = \"https://canvas.example.edu\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.canvas_url, "https://canvas.example.edu");
        assert!(config.panopto_host.is_empty());
        assert!(matches!(config.require(), Err(Error::NotConfigured)));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn missing_file_is_unconfigured_and_saving_round_trips() {
        let config = Config::load(Path::new("/no/such/canvas-cli.toml")).unwrap();
        assert!(!config.is_configured());
        let dir = std::env::temp_dir().join(format!("canvas-cli-save-{}", std::process::id()));
        let path = dir.join("config.toml");
        let set = Config {
            canvas_url: "https://canvas.example.edu".into(),
            panopto_host: "example.hosted.panopto.com".into(),
            ..Config::default()
        };
        set.save(&path).unwrap();
        assert_eq!(Config::load(&path).unwrap().require().unwrap(), set);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn addresses_normalise() {
        assert_eq!(
            canvas_origin(" https://canvas.example.edu/ ").unwrap(),
            "https://canvas.example.edu"
        );
        assert_eq!(
            canvas_origin("canvas.example.edu:8443/x").unwrap(),
            "https://canvas.example.edu:8443"
        );
        assert!(canvas_origin("").is_err());
        assert!(canvas_origin("ftp://canvas.example.edu").is_err());
        assert_eq!(
            recap_host("HTTPS://Example.Hosted.Panopto.com/Panopto/Pages/Home.aspx").unwrap(),
            "example.hosted.panopto.com"
        );
        assert!(recap_host("localhost").is_err());
        assert!(recap_host("not a url at all").is_err());
    }

    #[test]
    fn unknown_field_is_refused() {
        let err = toml::from_str::<Config>("token = \"x\"\n").unwrap_err();
        assert!(err.to_string().contains("unknown field"));
    }
}
