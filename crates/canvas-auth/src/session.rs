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

    /// Applies one `Set-Cookie` header received from `host`, as a browser
    /// would. Returns true if the jar changed.
    ///
    /// The cookie is ignored if its `Domain` does not cover `host` (a server
    /// may not set cookies for other sites). An empty value, `Max-Age=0` or
    /// a past `Expires` deletes it.
    ///
    /// # Example
    /// ```
    /// use canvas_auth::Session;
    /// let mut s = Session { captured_at: 0, cookies: vec![] };
    /// assert!(s.apply_set_cookie(".ASPXAUTH=new; path=/; secure; HttpOnly", "example.hosted.panopto.com", 0));
    /// assert_eq!(s.cookie_value("example.hosted.panopto.com", ".ASPXAUTH"), Some("new"));
    /// ```
    pub fn apply_set_cookie(&mut self, header: &str, host: &str, now_unix: i64) -> bool {
        let mut parts = header.split(';').map(str::trim);
        let Some((name, value)) = parts.next().and_then(|p| p.split_once('=')) else {
            return false;
        };
        let (name, value) = (name.trim(), value.trim().trim_matches('"'));
        if name.is_empty() {
            return false;
        }
        let mut domain = host.to_owned();
        let mut path = "/".to_owned();
        let mut expires: Option<f64> = None;
        let mut max_age: Option<i64> = None;
        for attr in parts {
            let (key, val) = attr
                .split_once('=')
                .map_or((attr, ""), |(k, v)| (k.trim(), v.trim()));
            match key.to_ascii_lowercase().as_str() {
                "domain" if !val.is_empty() => {
                    domain = val.trim_start_matches('.').to_ascii_lowercase()
                }
                "path" if !val.is_empty() => path = val.to_owned(),
                "max-age" => max_age = val.parse().ok(),
                "expires" => expires = parse_http_date(val),
                _ => {}
            }
        }
        // A server may only set cookies for itself or a parent domain, and
        // never for a bare top-level domain such as `eu`.
        let allowed =
            (host == domain || host.ends_with(&format!(".{domain}"))) && domain.contains('.');
        if !allowed {
            return false;
        }
        let expires = match max_age {
            Some(secs) => Some((now_unix + secs) as f64),
            None => expires,
        };
        let deleted = value.is_empty() || expires.is_some_and(|e| e <= now_unix as f64);
        let same = |c: &Cookie| {
            c.name == name && c.domain.trim_start_matches('.') == domain && c.path == path
        };
        let before = self.cookies.clone();
        self.cookies.retain(|c| !same(c));
        if !deleted {
            self.cookies.push(Cookie {
                name: name.to_owned(),
                value: value.to_owned(),
                domain,
                path,
                expires,
            });
        }
        self.cookies != before
    }

    /// Replaces every cookie for `host` with those in `other`, keeping all
    /// other hosts' cookies. Used to renew one site's login.
    pub fn replace_host(&mut self, host: &str, other: &Session) {
        self.cookies.retain(|c| !c.matches_host(host));
        self.cookies.extend(
            other
                .cookies
                .iter()
                .filter(|c| c.matches_host(host))
                .cloned(),
        );
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

/// Parses an HTTP cookie date into Unix seconds.
fn parse_http_date(text: &str) -> Option<f64> {
    chrono::DateTime::parse_from_rfc2822(text)
        .ok()
        .or_else(|| chrono::DateTime::parse_from_str(text, "%a, %d-%b-%Y %H:%M:%S GMT").ok())
        .or_else(|| {
            chrono::NaiveDateTime::parse_from_str(text, "%a, %d-%b-%Y %H:%M:%S GMT")
                .ok()
                .map(|n| n.and_utc().fixed_offset())
        })
        .map(|t| t.timestamp() as f64)
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
    fn set_cookie_replaces_deletes_and_refuses_other_sites() {
        let host = "example.hosted.panopto.com";
        let mut s = Session {
            captured_at: 0,
            cookies: vec![
                cookie(".ASPXAUTH", host, None),
                cookie("canvas_session", "canvas.example.edu", None),
            ],
        };
        assert!(s.apply_set_cookie(
            ".ASPXAUTH=fresh; expires=Thu, 05 Nov 2099 23:00:00 GMT; path=/; secure",
            host,
            1_000
        ));
        assert_eq!(s.cookie_value(host, ".ASPXAUTH"), Some("fresh"));
        assert_eq!(
            s.cookies.iter().filter(|c| c.name == ".ASPXAUTH").count(),
            1,
            "replaced, not duplicated"
        );
        assert!(s.earliest_expiry(host).is_some());
        // Same value again: no change.
        assert!(!s.apply_set_cookie(
            ".ASPXAUTH=fresh; expires=Thu, 05 Nov 2099 23:00:00 GMT; path=/",
            host,
            1_000
        ));
        // Another site's domain is refused.
        assert!(!s.apply_set_cookie("x=1; Domain=instructure.com", host, 1_000));
        assert!(!s.apply_set_cookie("x=1; Domain=evilpanopto.com", host, 1_000));
        assert!(!s.apply_set_cookie("x=1; Domain=com", host, 1_000));
        // Max-Age=0 deletes.
        assert!(s.apply_set_cookie(".ASPXAUTH=; Max-Age=0; path=/", host, 1_000));
        assert_eq!(s.cookie_value(host, ".ASPXAUTH"), None);
        assert_eq!(
            s.cookie_value("canvas.example.edu", "canvas_session"),
            Some("s3cret"),
            "other hosts untouched"
        );
    }

    #[test]
    fn replace_host_keeps_other_hosts() {
        let mut s = Session {
            captured_at: 0,
            cookies: vec![
                cookie("a", "example.hosted.panopto.com", None),
                cookie("b", "canvas.example.edu", None),
            ],
        };
        let fresh = Session {
            captured_at: 1,
            cookies: vec![
                cookie("c", "example.hosted.panopto.com", None),
                cookie("d", "canvas.example.edu", None),
            ],
        };
        s.replace_host("example.hosted.panopto.com", &fresh);
        let names: Vec<_> = s.cookies.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["b", "c"]);
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
