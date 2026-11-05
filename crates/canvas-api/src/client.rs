//! The HTTP client and its session handling.

use canvas_auth::Session;
use canvas_core::{Config, Error, Result};
use reqwest::{StatusCode, header, redirect};
use serde::de::DeserializeOwned;

use crate::User;

/// A read-only Canvas API client bound to one session.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: url::Url,
    cookie: String,
}

impl Client {
    /// Builds a client for `config.canvas_url` using `session`'s cookies.
    ///
    /// # Errors
    /// Returns [`Error::SessionExpired`] if the session has no Canvas
    /// cookies, and [`Error::Http`] if the base URL is invalid.
    pub fn new(config: &Config, session: &Session) -> Result<Self> {
        let base = url::Url::parse(&config.canvas_url)
            .map_err(|e| Error::Http(format!("bad canvas_url: {e}")))?;
        let host = base.host_str().unwrap_or_default();
        let cookie = session.cookie_header(host).ok_or(Error::SessionExpired)?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("canvas-cli/", env!("CARGO_PKG_VERSION")))
            // A redirect from the API means "go and log in"; we report that
            // instead of following it to an HTML page.
            .redirect(redirect::Policy::none())
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(Self { http, base, cookie })
    }

    /// Returns the signed-in user. Used to check that a session works.
    ///
    /// # Errors
    /// Returns [`Error::SessionExpired`] if Canvas rejects the session.
    pub async fn current_user(&self) -> Result<User> {
        self.get("/api/v1/users/self").await
    }

    /// Sends `GET path` and decodes the JSON body.
    ///
    /// # Errors
    /// Returns [`Error::SessionExpired`] on 401 or a redirect,
    /// [`Error::Http`] on other failures, and [`Error::Decode`] if the
    /// body is not the expected JSON.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self
            .base
            .join(path)
            .map_err(|e| Error::Http(e.to_string()))?;
        let response = self
            .http
            .get(url)
            .header(header::COOKIE, &self.cookie)
            .header(header::ACCEPT, "application/json")
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        let status = response.status();
        if status == StatusCode::UNAUTHORIZED || status.is_redirection() {
            return Err(Error::SessionExpired);
        }
        if !status.is_success() {
            return Err(Error::Http(format!("GET {path} returned {status}")));
        }
        let text = response
            .text()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        // Canvas may prefix JSON with `while(1);` to block JSON hijacking.
        let body = text.strip_prefix("while(1);").unwrap_or(&text);
        serde_json::from_str(body).map_err(|e| Error::Decode {
            what: path.to_owned(),
            detail: e.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_auth::Cookie;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves one canned HTTP response on a loopback port and returns the
    /// base URL plus a handle yielding the raw request it received.
    async fn serve_once(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            socket.write_all(response.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&buf[..n]).into_owned()
        });
        (base, handle)
    }

    fn client(base: &str) -> Client {
        let config = Config {
            canvas_url: base.to_owned(),
            ..Config::default()
        };
        let session = Session {
            captured_at: 0,
            cookies: vec![Cookie {
                name: "canvas_session".into(),
                value: "abc".into(),
                domain: "127.0.0.1".into(),
                path: "/".into(),
                expires: None,
            }],
        };
        Client::new(&config, &session).unwrap()
    }

    #[tokio::test]
    async fn sends_cookie_and_strips_json_guard() {
        let (base, request) = serve_once(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 44\r\nConnection: close\r\n\r\nwhile(1);{\"id\":7,\"name\":\"A\",\"login_id\":null}",
        )
        .await;
        let user = client(&base).current_user().await.unwrap();
        assert_eq!(user.id, 7);
        let request = request.await.unwrap();
        assert!(request.starts_with("GET /api/v1/users/self "));
        assert!(
            request
                .to_lowercase()
                .contains("cookie: canvas_session=abc")
        );
    }

    #[tokio::test]
    async fn unauthorized_means_session_expired() {
        let (base, _) = serve_once(
            "HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let err = client(&base).current_user().await.unwrap_err();
        assert!(matches!(err, Error::SessionExpired));
    }

    #[tokio::test]
    async fn login_redirect_means_session_expired() {
        let (base, _) = serve_once(
            "HTTP/1.1 302 Found\r\nLocation: /login\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        )
        .await;
        let err = client(&base).current_user().await.unwrap_err();
        assert!(matches!(err, Error::SessionExpired));
    }

    #[test]
    fn no_canvas_cookie_means_session_expired() {
        let config = Config::default();
        let session = Session {
            captured_at: 0,
            cookies: vec![],
        };
        assert!(matches!(
            Client::new(&config, &session),
            Err(Error::SessionExpired)
        ));
    }
}
