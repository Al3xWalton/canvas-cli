//! The HTTP client and its session handling.

use canvas_auth::Session;
use canvas_core::{Config, Error, Result};
use reqwest::{StatusCode, header, redirect};
use serde::de::DeserializeOwned;

use crate::{Assignment, Course, User};

/// Attempts per request, counting the first, before a rate limit is fatal.
const MAX_ATTEMPTS: u32 = 4;
/// Pages followed per list before giving up, as a guard against loops.
const MAX_PAGES: usize = 200;
/// Longest single rate-limit wait we accept, whatever `Retry-After` says.
const MAX_WAIT_SECS: u64 = 60;

/// A read-only Canvas API client bound to one session.
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
    base: url::Url,
    cookie: String,
    backoff: std::time::Duration,
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
        let loopback = matches!(host, "127.0.0.1" | "localhost" | "[::1]");
        if base.scheme() != "https" && !loopback {
            return Err(Error::Http(format!(
                "canvas_url must use https (got {}); refusing to send the session in cleartext",
                base.scheme()
            )));
        }
        let cookie = session.cookie_header(host).ok_or(Error::SessionExpired)?;
        let http = reqwest::Client::builder()
            .user_agent(concat!("canvas-cli/", env!("CARGO_PKG_VERSION")))
            // A redirect from the API means "go and log in"; we report that
            // instead of following it to an HTML page.
            .redirect(redirect::Policy::none())
            // An unanswered request must fail, not hang: the MCP server
            // checks the session before its handshake.
            .connect_timeout(std::time::Duration::from_secs(10))
            .timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(Self {
            http,
            base,
            cookie,
            backoff: std::time::Duration::from_secs(1),
        })
    }

    /// Returns the signed-in user. Used to check that a session works.
    ///
    /// # Errors
    /// Returns [`Error::SessionExpired`] if Canvas rejects the session.
    pub async fn current_user(&self) -> Result<User> {
        self.get("/api/v1/users/self").await
    }

    /// Lists the user's active courses, with their terms.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    ///
    /// Courses Canvas hides by date (`access_restricted_by_date`) carry no
    /// details and are skipped.
    pub async fn courses(&self) -> Result<Vec<Course>> {
        let courses: Vec<Course> = self
            .get_all("/api/v1/courses?enrollment_state=active&include[]=term&per_page=100")
            .await?;
        Ok(courses
            .into_iter()
            .filter(|c| !c.access_restricted_by_date)
            .collect())
    }

    /// Lists every assignment in a course, ordered by due date, with the
    /// user's own submission attached.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    pub async fn assignments(&self, course_id: u64) -> Result<Vec<Assignment>> {
        self.get_all(&format!(
            "/api/v1/courses/{course_id}/assignments?order_by=due_at&include[]=submission&per_page=100"
        ))
        .await
    }

    /// Sends `GET path` and decodes the JSON body.
    ///
    /// # Errors
    /// Returns [`Error::SessionExpired`] on 401 or a redirect,
    /// [`Error::Http`] on other failures, and [`Error::Decode`] if the
    /// body is not the expected JSON.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<T> {
        let url = self.url(path)?;
        let (body, _) = self.fetch(url, path).await?;
        decode(&body, path)
    }

    /// Sends `GET path` and follows `rel="next"` links, concatenating
    /// every page.
    ///
    /// # Errors
    /// As for [`Client::get`], plus [`Error::Http`] if a next link points
    /// off the Canvas host or more than 200 pages are returned.
    pub async fn get_all<T: DeserializeOwned>(&self, path: &str) -> Result<Vec<T>> {
        let mut out = Vec::new();
        let mut next = Some(self.url(path)?);
        let mut pages = 0;
        while let Some(url) = next.take() {
            pages += 1;
            if pages > MAX_PAGES {
                return Err(Error::Http(format!("{path}: more than {MAX_PAGES} pages")));
            }
            let (body, link) = self.fetch(url, path).await?;
            out.extend(decode::<Vec<T>>(&body, path)?);
            if let Some(link) = link.as_deref().and_then(next_link) {
                let link = url::Url::parse(link).map_err(|e| Error::Http(e.to_string()))?;
                if link.origin() != self.base.origin() {
                    return Err(Error::Http(format!(
                        "{path}: next page points to another host; refusing to send cookies"
                    )));
                }
                next = Some(link);
            }
        }
        Ok(out)
    }

    fn url(&self, path: &str) -> Result<url::Url> {
        self.base.join(path).map_err(|e| Error::Http(e.to_string()))
    }

    /// Fetches one URL with rate-limit retries, returning the body and the
    /// raw `Link` header.
    async fn fetch(&self, url: url::Url, what: &str) -> Result<(String, Option<String>)> {
        let mut attempt = 0;
        loop {
            attempt += 1;
            let response = self
                .http
                .get(url.clone())
                .header(header::COOKIE, &self.cookie)
                .header(header::ACCEPT, "application/json")
                .send()
                .await
                .map_err(|e| Error::Http(e.to_string()))?;
            let status = response.status();
            if status == StatusCode::UNAUTHORIZED || status.is_redirection() {
                return Err(Error::SessionExpired);
            }
            let retry_after = response
                .headers()
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse::<u64>().ok());
            let link = response
                .headers()
                .get(header::LINK)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned);
            let text = response
                .text()
                .await
                .map_err(|e| Error::Http(e.to_string()))?;
            let limited = status == StatusCode::TOO_MANY_REQUESTS
                || (status == StatusCode::FORBIDDEN && text.contains("Rate Limit Exceeded"));
            if limited && attempt < MAX_ATTEMPTS {
                if retry_after.is_some_and(|secs| secs > MAX_WAIT_SECS) {
                    return Err(Error::Http(format!(
                        "GET {what} is rate limited for longer than {MAX_WAIT_SECS}s; try again later"
                    )));
                }
                let wait = retry_after
                    .map(std::time::Duration::from_secs)
                    .unwrap_or(self.backoff * 2u32.pow(attempt - 1));
                tokio::time::sleep(wait).await;
                continue;
            }
            if !status.is_success() {
                return Err(Error::Http(format!("GET {what} returned {status}")));
            }
            return Ok((text, link));
        }
    }
}

/// Decodes a Canvas JSON body, removing the `while(1);` hijacking guard.
fn decode<T: DeserializeOwned>(text: &str, what: &str) -> Result<T> {
    let body = text.strip_prefix("while(1);").unwrap_or(text);
    serde_json::from_str(body).map_err(|e| Error::Decode {
        what: what.to_owned(),
        detail: e.to_string(),
    })
}

/// Returns the `rel="next"` URL from a `Link` header, if present.
fn next_link(header: &str) -> Option<&str> {
    header.split(',').find_map(|part| {
        let (url, params) = part.split_once(';')?;
        params
            .split(';')
            .filter_map(|p| p.trim().split_once('='))
            .filter(|(k, _)| k.trim().eq_ignore_ascii_case("rel"))
            .any(|(_, v)| {
                v.trim()
                    .trim_matches('"')
                    .split_whitespace()
                    .any(|r| r.eq_ignore_ascii_case("next"))
            })
            .then(|| url.trim().trim_start_matches('<').trim_end_matches('>'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_auth::Cookie;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Serves one canned HTTP response on a loopback port and returns the
    /// base URL plus a handle yielding the raw request it received.
    async fn serve_once(response: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let (base, handle) = serve(vec![response.to_owned()]).await;
        (
            base,
            tokio::spawn(async move { handle.await.unwrap().remove(0) }),
        )
    }

    /// Serves `responses` in order, one per connection. `{base}` in a
    /// response is replaced with the server's own URL. The handle yields
    /// every raw request received.
    async fn serve(responses: Vec<String>) -> (String, tokio::task::JoinHandle<Vec<String>>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let me = base.clone();
        let handle = tokio::spawn(async move {
            let mut seen = Vec::new();
            for response in responses {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut buf = vec![0; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                seen.push(String::from_utf8_lossy(&buf[..n]).into_owned());
                let response = response.replace("{base}", &me);
                socket.write_all(response.as_bytes()).await.unwrap();
            }
            seen
        });
        (base, handle)
    }

    fn ok_json(body: &str, link: Option<&str>) -> String {
        let link = link.map(|l| format!("Link: {l}\r\n")).unwrap_or_default();
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{link}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
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
        let mut client = Client::new(&config, &session).unwrap();
        client.backoff = std::time::Duration::from_millis(1);
        client
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

    #[tokio::test]
    async fn follows_next_links_across_pages() {
        let (base, requests) = serve(vec![
            ok_json(
                r#"[{"id":1,"name":"A"}]"#,
                Some(r#"<{base}/api/v1/courses?page=2>; rel="next", <{base}/api/v1/courses?page=1>; rel="first""#),
            ),
            ok_json(r#"[{"id":2,"name":"B","course_code":"B1"}]"#, None),
        ])
        .await;
        let courses = client(&base).courses().await.unwrap();
        assert_eq!(courses.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1, 2]);
        let requests = requests.await.unwrap();
        assert!(requests[1].starts_with("GET /api/v1/courses?page=2 "));
        assert!(requests[1].contains("canvas_session=abc"));
    }

    #[tokio::test]
    async fn refuses_next_link_to_another_host() {
        let (base, _) = serve(vec![ok_json(
            r#"[{"id":1,"name":"A"}]"#,
            Some(r#"<https://evil.example/steal?page=2>; rel="next""#),
        )])
        .await;
        let err = client(&base).courses().await.unwrap_err();
        assert!(err.to_string().contains("another host"));
    }

    #[tokio::test]
    async fn retries_rate_limits_then_succeeds() {
        let limited = "HTTP/1.1 403 Forbidden\r\nContent-Length: 19\r\nConnection: close\r\n\r\nRate Limit Exceeded".to_owned();
        let (base, requests) =
            serve(vec![limited, ok_json(r#"[{"id":3,"name":"C"}]"#, None)]).await;
        let courses = client(&base).courses().await.unwrap();
        assert_eq!(courses[0].id, 3);
        assert_eq!(requests.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn plain_forbidden_is_not_retried() {
        let (base, requests) = serve(vec![
            "HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
        ])
        .await;
        assert!(matches!(client(&base).courses().await, Err(Error::Http(_))));
        assert_eq!(requests.await.unwrap().len(), 1);
    }

    #[test]
    fn parses_next_link_among_others() {
        let h = r#"<https://x/a?page=1>; rel="current", <https://x/a?page=2>; rel="next""#;
        assert_eq!(next_link(h), Some("https://x/a?page=2"));
        assert_eq!(next_link(r#"<https://x/a>; rel="last""#), None);
        assert_eq!(next_link("<https://x/b>; rel=next"), Some("https://x/b"));
        assert_eq!(
            next_link(r#"<https://x/c>;  REL="next last""#),
            Some("https://x/c")
        );
    }

    #[tokio::test]
    async fn hostile_retry_after_fails_fast() {
        let (base, _) = serve(vec![
            "HTTP/1.1 429 Too Many Requests\r\nRetry-After: 99999999\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_owned(),
        ])
        .await;
        let err = client(&base).courses().await.unwrap_err();
        assert!(err.to_string().contains("rate limited"));
    }

    #[test]
    fn plain_http_canvas_url_is_refused() {
        let config = Config {
            canvas_url: "http://canvas.example.edu".into(),
            ..Config::default()
        };
        let session = Session {
            captured_at: 0,
            cookies: vec![Cookie {
                name: "canvas_session".into(),
                value: "abc".into(),
                domain: "canvas.example.edu".into(),
                path: "/".into(),
                expires: None,
            }],
        };
        let err = Client::new(&config, &session).unwrap_err();
        assert!(err.to_string().contains("https"));
    }

    #[tokio::test]
    async fn date_restricted_courses_do_not_break_the_list() {
        let (base, _) = serve(vec![ok_json(
            r#"[{"id":1,"name":"A"},{"id":2,"access_restricted_by_date":true}]"#,
            None,
        )])
        .await;
        let courses = client(&base).courses().await.unwrap();
        assert_eq!(courses.iter().map(|c| c.id).collect::<Vec<_>>(), vec![1]);
    }
}
