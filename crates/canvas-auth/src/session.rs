//! The captured cookie jar and its keychain storage.

use std::fmt;

use canvas_core::{Error, Result};
use serde::{Deserialize, Serialize};

/// Keychain service name for the stored session.
const SERVICE: &str = "canvas-cli";
/// Keychain account name for the stored session.
const ACCOUNT: &str = "browser-session";
/// Overrides [`ACCOUNT`], so tests and second profiles never touch the
/// user's real session.
const ACCOUNT_ENV: &str = "CANVAS_CLI_KEYCHAIN_ACCOUNT";

/// One cookie captured from the sign-in browser.
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct Cookie {
    /// Cookie name, e.g. `canvas_session`.
    pub name: String,
    /// Cookie value. Secret.
    pub value: String,
    /// Domain the cookie was set for, e.g. `canvas.example.edu`.
    pub domain: String,
    /// Path the cookie applies to.
    pub path: String,
    /// Expiry as Unix seconds, or `None` for a browser-session cookie.
    pub expires: Option<f64>,
}

impl fmt::Debug for Cookie {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Cookie")
            .field("name", &self.name)
            .field("value", &"<redacted>")
            .field("domain", &self.domain)
            .field("path", &self.path)
            .field("expires", &self.expires)
            .finish()
    }
}

impl Cookie {
    /// Returns true if this cookie would be sent to `host`.
    ///
    /// Follows the browser rule: a domain of `.example.com` or
    /// `example.com` matches `example.com` and any subdomain of it.
    pub fn matches_host(&self, host: &str) -> bool {
        let domain = self.domain.trim_start_matches('.');
        host == domain || host.ends_with(&format!(".{domain}"))
    }
}

/// Every cookie Canvas CLI needs, captured at one sign-in.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    /// Unix seconds at which the session was captured.
    pub captured_at: u64,
    /// Cookies for the Canvas and Panopto hosts.
    pub cookies: Vec<Cookie>,
}

impl Session {
    /// Builds the `Cookie` request header for `host`, or `None` if no
    /// cookie applies to it.
    ///
    /// # Example
    /// ```
    /// use canvas_auth::{Cookie, Session};
    /// let session = Session {
    ///     captured_at: 0,
    ///     cookies: vec![Cookie {
    ///         name: "a".into(), value: "1".into(),
    ///         domain: ".instructure.com".into(), path: "/".into(), expires: None,
    ///     }],
    /// };
    /// assert_eq!(session.cookie_header("canvas.example.edu").as_deref(), Some("a=1"));
    /// assert_eq!(session.cookie_header("example.org"), None);
    /// ```
    pub fn cookie_header(&self, host: &str) -> Option<String> {
        let pairs: Vec<String> = self
            .cookies
            .iter()
            .filter(|c| c.matches_host(host))
            .map(|c| format!("{}={}", c.name, c.value))
            .collect();
        (!pairs.is_empty()).then(|| pairs.join("; "))
    }

    /// Returns the value of the cookie called `name` that applies to
    /// `host`, if any. Used for CSRF tokens carried in cookies.
    pub fn cookie_value(&self, host: &str, name: &str) -> Option<&str> {
        self.cookies
            .iter()
            .find(|c| c.name == name && c.matches_host(host))
            .map(|c| c.value.as_str())
    }

    /// Returns true if any cookie applies to `host`.
    pub fn covers(&self, host: &str) -> bool {
        self.cookies.iter().any(|c| c.matches_host(host))
    }

    /// Returns the earliest expiry (Unix seconds) among `host`'s cookies
    /// that have one, or `None` if all are browser-session cookies.
    pub fn earliest_expiry(&self, host: &str) -> Option<f64> {
        self.cookies
            .iter()
            .filter(|c| c.matches_host(host))
            .filter_map(|c| c.expires)
            .reduce(f64::min)
    }
}

fn entry() -> Result<keyring::Entry> {
    let account = std::env::var(ACCOUNT_ENV).unwrap_or_else(|_| ACCOUNT.to_owned());
    keyring::Entry::new(SERVICE, &account).map_err(|e| Error::Keychain(e.to_string()))
}

/// Saves `session` to the OS keychain, replacing any earlier one.
///
/// # Errors
/// Returns [`Error::Keychain`] if the keychain refuses the write.
pub fn store(session: &Session) -> Result<()> {
    let json = serde_json::to_string(session).map_err(|e| Error::Keychain(e.to_string()))?;
    entry()?
        .set_password(&json)
        .map_err(|e| Error::Keychain(e.to_string()))
}

/// Loads the stored session.
///
/// # Errors
/// Returns [`Error::SessionExpired`] if there is none, and
/// [`Error::Keychain`] if the keychain cannot be read or the entry is
/// corrupt.
pub fn load() -> Result<Session> {
    match entry()?.get_password() {
        Ok(json) => serde_json::from_str(&json).map_err(|e| Error::Keychain(e.to_string())),
        Err(keyring::Error::NoEntry) => Err(Error::SessionExpired),
        Err(e) => Err(Error::Keychain(e.to_string())),
    }
}

/// Removes the stored session. Succeeds if there was none.
///
/// # Errors
/// Returns [`Error::Keychain`] if the keychain refuses the delete.
pub fn delete() -> Result<()> {
    match entry()?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(Error::Keychain(e.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cookie(name: &str, domain: &str, expires: Option<f64>) -> Cookie {
        Cookie {
            name: name.into(),
            value: "s3cret".into(),
            domain: domain.into(),
            path: "/".into(),
            expires,
        }
    }

    #[test]
    fn debug_never_prints_cookie_values() {
        let session = Session {
            captured_at: 1,
            cookies: vec![cookie("canvas_session", "canvas.example.edu", None)],
        };
        let shown = format!("{session:?}");
        assert!(!shown.contains("s3cret"));
        assert!(shown.contains("<redacted>"));
    }

    #[test]
    fn host_matching_follows_browser_rules() {
        let c = cookie("a", ".panopto.com", None);
        assert!(c.matches_host("example.hosted.panopto.com"));
        assert!(c.matches_host("panopto.com"));
        assert!(!c.matches_host("evilpanopto.com"));
        assert!(!cookie("b", "canvas.example.edu", None).matches_host("instructure.com"));
    }

    #[test]
    fn earliest_expiry_ignores_other_hosts_and_session_cookies() {
        let session = Session {
            captured_at: 0,
            cookies: vec![
                cookie("a", "canvas.example.edu", Some(200.0)),
                cookie("b", "canvas.example.edu", Some(100.0)),
                cookie("c", "canvas.example.edu", None),
                cookie("d", "example.hosted.panopto.com", Some(50.0)),
            ],
        };
        assert_eq!(session.earliest_expiry("canvas.example.edu"), Some(100.0));
    }
}
