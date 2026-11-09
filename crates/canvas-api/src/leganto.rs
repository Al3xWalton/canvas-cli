//! Ex Libris Leganto reading lists, reached through Canvas.
//!
//! Leganto has no public API for students, but a course's "Reading List"
//! menu item is an LTI 1.3 tool, so the user's Canvas session can open it
//! the way a browser would:
//!
//! 1. Canvas renders the launch form (`/courses/<id>/external_tools/<tool>`).
//! 2. It is posted to Alma's LTI login, which redirects back to Canvas's
//!    authorize endpoint; Canvas answers with a form carrying a signed
//!    `id_token`, which is posted to Alma.
//! 3. Alma starts a Leganto session (cookies) and redirects to the list.
//!
//! The list is then read from the JSON endpoints Leganto's own page uses:
//! `GET /leganto/rl/main/lists/<id>` (sections and citations) and
//! `POST /leganto/rl/main/lists/<id>/additionalinfo` (tags such as
//! Essential or Recommended). Neither changes anything.
//!
//! Cookies live in a private jar copied from the session for this one
//! launch. Canvas cookies are only ever sent to the Canvas host; Leganto's
//! cookies are dropped when the call ends and never stored or logged.

use canvas_auth::Session;
use canvas_core::{Error, Result};
use regex::Regex;
use reqwest::{Method, StatusCode, header, redirect};
use serde_json::Value;
use std::sync::LazyLock;

use crate::readings::importance_in;
use crate::{Client, Reading, ReadingSection};

/// Most redirects and form posts in one launch.
const MAX_HOPS: usize = 16;

static FORM: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(?is)<form\b([^>]*)>(.*?)</form>"#).ok());
static INPUT: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r#"(?is)<input\b[^>]*>"#).ok());
static ATTR: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(?is)\b([a-z_-]+)\s*=\s*(?:"([^"]*)"|'([^']*)')"#).ok());
static LIST_ID: LazyLock<Option<Regex>> = LazyLock::new(|| Regex::new(r"/lists/(\d{6,})").ok());
static JS_REDIRECT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r#"(?is)(?:location(?:\.href)?\s*=|location\.replace\(|http-equiv=["']refresh["'][^>]*url=)\s*["']?([^"'\s;)>]+)"#).ok()
});

/// Values, queries and long tokens, hidden from debug output.
static REDACT: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(r#"(?s)(value|content)=("[^"]*"|'[^']*')|[?#][^"'\s<]*|[A-Za-z0-9_\-.]{24,}"#).ok()
});

/// One HTML form: where it posts and its fields.
#[derive(Debug, PartialEq)]
struct Form {
    action: String,
    method: Method,
    fields: Vec<(String, String)>,
}

fn attrs(tag: &str) -> Vec<(String, String)> {
    ATTR.as_ref().map_or_else(Vec::new, |re| {
        re.captures_iter(tag)
            .map(|c| {
                let v = c.get(2).or_else(|| c.get(3)).map_or("", |m| m.as_str());
                (c[1].to_ascii_lowercase(), unescape(v))
            })
            .collect()
    })
}

fn unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&#x27;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#x2F;", "/")
}

/// The LTI hand-off form in a page: the first form whose fields include
/// an LTI or OIDC parameter.
fn lti_form(html: &str) -> Option<Form> {
    const MARKERS: &[&str] = &[
        "id_token",
        "lti_message_hint",
        "login_hint",
        "jwt",
        "samlresponse",
        "oauth_signature",
    ];
    let forms = FORM.as_ref()?;
    let inputs = INPUT.as_ref()?;
    for f in forms.captures_iter(html) {
        let head = attrs(&f[1]);
        let get = |k: &str| head.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        // No action means "post back to this page" (e.g. a cookie check).
        let action = get("action").unwrap_or_default();
        let fields: Vec<(String, String)> = inputs
            .find_iter(&f[2])
            .filter_map(|i| {
                let a = attrs(i.as_str());
                let name = a.iter().find(|(n, _)| n == "name")?.1.clone();
                let value = a
                    .iter()
                    .find(|(n, _)| n == "value")
                    .map_or_else(String::new, |(_, v)| v.clone());
                (!name.is_empty()).then_some((name, value))
            })
            .collect();
        if fields
            .iter()
            .any(|(n, _)| MARKERS.contains(&n.to_ascii_lowercase().as_str()))
        {
            let method = if get("method").is_some_and(|m| m.eq_ignore_ascii_case("get")) {
                Method::GET
            } else {
                Method::POST
            };
            return Some(Form {
                action,
                method,
                fields,
            });
        }
    }
    None
}

/// What one launch found.
pub(crate) struct Launched {
    /// Leganto origin, e.g. `https://ncl.alma.exlibrisgroup.com`.
    pub origin: url::Url,
    /// Lists reached, by id.
    pub list_ids: Vec<String>,
    jar: Session,
    http: reqwest::Client,
}

fn trace(step: &str, url: &url::Url, status: Option<StatusCode>) {
    if std::env::var_os("CANVAS_DEBUG_LTI").is_some() {
        // Host and path only: queries carry tokens.
        eprintln!(
            "lti: {step} {}{} {}",
            url.host_str().unwrap_or_default(),
            url.path(),
            status.map_or_else(String::new, |s| s.as_u16().to_string())
        );
    }
}

impl Client {
    /// Launches the Canvas LTI tool at `tool_url` (a Canvas
    /// `external_tools` link) and follows it to Leganto.
    ///
    /// # Errors
    /// [`Error::Http`] if the tool is not on this Canvas, the hand-off
    /// breaks, or it does not end at Leganto; [`Error::SessionExpired`] if
    /// Canvas wants a sign-in.
    pub(crate) async fn launch_leganto(&self, tool_url: &str) -> Result<Launched> {
        let canvas_host = self.base.host_str().unwrap_or_default().to_owned();
        let mut url = url::Url::parse(tool_url).map_err(|e| Error::Http(e.to_string()))?;
        if url.host_str() != Some(canvas_host.as_str()) || !url.path().contains("/external_tools/")
        {
            return Err(Error::Http("not a Canvas course tool link".into()));
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("canvas-cli/", env!("CARGO_PKG_VERSION")))
            .redirect(redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        // Only Canvas cookies start in the jar.
        let mut jar = Session {
            captured_at: 0,
            cookies: self
                .session
                .cookies
                .iter()
                .filter(|c| c.matches_host(&canvas_host))
                .cloned()
                .collect(),
        };
        let mut method = Method::GET;
        let mut fields: Vec<(String, String)> = Vec::new();
        for _ in 0..MAX_HOPS {
            if url.scheme() != "https" && url.host_str() != Some("127.0.0.1") {
                return Err(Error::Http("tool launch left https".into()));
            }
            let host = url.host_str().unwrap_or_default().to_owned();
            let mut req = http.request(method.clone(), url.clone());
            if let Some(c) = jar.cookie_header(&host) {
                req = req.header(header::COOKIE, c);
            }
            if method == Method::POST {
                req = req.form(&fields);
            }
            let resp = req.send().await.map_err(|e| Error::Http(e.to_string()))?;
            let status = resp.status();
            trace(method.as_str(), &url, Some(status));
            let now = chrono::Utc::now().timestamp();
            for v in resp.headers().get_all(header::SET_COOKIE) {
                if let Ok(line) = v.to_str() {
                    jar.apply_set_cookie(line, &host, now);
                }
            }
            if status.is_redirection() {
                let loc = resp
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|l| l.to_str().ok())
                    .ok_or_else(|| Error::Http("redirect without a location".into()))?;
                let next = url.join(loc).map_err(|e| Error::Http(e.to_string()))?;
                if next.host_str() == Some(canvas_host.as_str())
                    && next.path().starts_with("/login")
                {
                    return Err(Error::SessionExpired);
                }
                url = next;
                method = Method::GET;
                fields.clear();
                continue;
            }
            if status == StatusCode::UNAUTHORIZED && host == canvas_host {
                return Err(Error::SessionExpired);
            }
            if !status.is_success() {
                return Err(Error::Http(format!(
                    "reading-list launch: {host} answered {status}"
                )));
            }
            let body = resp.text().await.map_err(|e| Error::Http(e.to_string()))?;
            if url.path().contains("/leganto/") && host != canvas_host {
                let mut list_ids: Vec<String> = Vec::new();
                if let Some(re) = LIST_ID.as_ref() {
                    for c in re.captures_iter(url.path()).chain(re.captures_iter(&body)) {
                        if !list_ids.contains(&c[1].to_owned()) {
                            list_ids.push(c[1].to_owned());
                        }
                    }
                }
                let mut origin = url.clone();
                origin.set_path("/");
                origin.set_query(None);
                return Ok(Launched {
                    origin,
                    list_ids,
                    jar,
                    http,
                });
            }
            if let Some(form) = lti_form(&body) {
                url = url
                    .join(&form.action)
                    .map_err(|e| Error::Http(e.to_string()))?;
                method = form.method;
                fields = form.fields;
                if method == Method::GET {
                    url.query_pairs_mut().extend_pairs(fields.drain(..));
                }
                continue;
            }
            if let Some(next) = JS_REDIRECT
                .as_ref()
                .and_then(|re| re.captures(&body))
                .and_then(|c| url.join(&unescape(&c[1])).ok())
            {
                url = next;
                method = Method::GET;
                fields.clear();
                continue;
            }
            if std::env::var_os("CANVAS_DEBUG_LTI").is_some() {
                let redacted = REDACT
                    .as_ref()
                    .map(|re| re.replace_all(&body, "$1=…").into_owned())
                    .unwrap_or_default();
                eprintln!(
                    "lti: body ({} bytes):\n{}",
                    body.len(),
                    redacted.chars().take(4000).collect::<String>()
                );
            }
            return Err(Error::Http(format!(
                "reading-list launch stopped at {host}{} (no hand-off found)",
                url.path()
            )));
        }
        Err(Error::Http(
            "reading-list launch: too many redirects".into(),
        ))
    }
}

impl Launched {
    async fn json(&self, method: Method, path: &str, body: Option<&Value>) -> Result<Value> {
        let url = self
            .origin
            .join(path)
            .map_err(|e| Error::Http(e.to_string()))?;
        let host = url.host_str().unwrap_or_default().to_owned();
        let mut req = self
            .http
            .request(method, url)
            .header(header::ACCEPT, "application/json");
        if let Some(c) = self.jar.cookie_header(&host) {
            req = req.header(header::COOKIE, c);
        }
        if let Some(b) = body {
            req = req.json(b);
        }
        let resp = req.send().await.map_err(|e| Error::Http(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Http(format!("Leganto {path}: {status}")));
        }
        resp.json().await.map_err(|e| Error::Decode {
            what: format!("Leganto {path}"),
            detail: e.to_string(),
        })
    }

    /// Reads one list as sections, one per Leganto section.
    pub(crate) async fn read_list(&self, list_id: &str) -> Result<(String, Vec<ReadingSection>)> {
        let list = self
            .json(
                Method::GET,
                &format!("leganto/rl/main/lists/{list_id}"),
                None,
            )
            .await?;
        let list = &list["object"];
        let mut body = serde_json::json!({"citations": [], "sections": []});
        for s in list["sections"].as_array().into_iter().flatten() {
            if let Some(a) = body["sections"].as_array_mut() {
                a.push(s["id"].clone());
            }
            for c in s["citations"].as_array().into_iter().flatten() {
                if let Some(a) = body["citations"].as_array_mut() {
                    a.push(c["id"].clone());
                }
            }
        }
        // Tags are extra: a failure here still returns the list.
        let extra = self
            .json(
                Method::POST,
                &format!("leganto/rl/main/lists/{list_id}/additionalinfo"),
                Some(&body),
            )
            .await
            .unwrap_or(Value::Null);
        let page = self
            .origin
            .join(&format!("leganto/nui/lists/{list_id}"))
            .map(String::from)
            .ok();
        Ok((
            text(&list["name"]),
            sections_from(list, &extra, page.as_deref()),
        ))
    }
}

/// A JSON value as text: strings as-is, arrays joined, numbers printed.
fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.trim().to_owned(),
        Value::Number(n) => n.to_string(),
        Value::Array(a) => a
            .iter()
            .map(text)
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("; "),
        _ => String::new(),
    }
}

/// Maps Leganto's list JSON (and the tags from `additionalinfo`) to
/// reading sections.
pub(crate) fn sections_from(
    list: &Value,
    extra: &Value,
    page: Option<&str>,
) -> Vec<ReadingSection> {
    let name = text(&list["name"]);
    let mut out = Vec::new();
    for s in list["sections"].as_array().into_iter().flatten() {
        // "Core reading" or "Recommended reading" sections mark untagged items.
        let section_importance = importance_in(&text(&s["name"]));
        let items = s["citations"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|c| {
                let mut r = citation(c, &extra["citations"][text(&c["id"])], page);
                if r.importance.is_none() {
                    r.importance = section_importance.map(str::to_owned);
                }
                r
            })
            .collect();
        out.push(ReadingSection {
            module: format!("Reading list: {name}"),
            heading: text(&s["name"]),
            source: "leganto".into(),
            text: crate::html_to_markdown(&text(&s["description"])),
            items,
            ..Default::default()
        });
    }
    out
}

fn citation(c: &Value, extra: &Value, page: Option<&str>) -> Reading {
    let dc = &c["dc"];
    let get = |k: &str| text(&dc[k]);
    let kind = match (
        text(&c["type"]).as_str(),
        text(&c["secondaryType"]).as_str(),
    ) {
        (_, "BK_C") => "chapter",
        ("BK", _) => "book",
        ("CR", _) if !get("rlterms_journalTitle").is_empty() || !get("doi").is_empty() => "article",
        ("CR", _) => "article",
        ("WS", _) => "web",
        ("VD" | "AV", _) => "video",
        _ => "unknown",
    };
    let mut details: Vec<String> = Vec::new();
    let chapter_of = get("rlterms_chapter_title");
    let journal = get("rlterms_journalTitle");
    for d in [
        if journal.is_empty() {
            String::new()
        } else {
            journal
        },
        get("rlterms_edition").replace("Ed.", "ed."),
        {
            let v = get("rlterms_volumePartNumber");
            let v = if v.is_empty() { get("volume") } else { v };
            if v.is_empty() { v } else { format!("vol. {v}") }
        },
        {
            let i = get("rlterms_issue");
            if i.is_empty() {
                i
            } else {
                format!("issue {i}")
            }
        },
        {
            let (a, b, p) = (get("start_page"), get("end_page"), get("rlterms_pages"));
            match (a.is_empty(), b.is_empty()) {
                (false, false) => format!("pp. {a}-{b}"),
                (false, true) => format!("p. {a}"),
                _ if !p.is_empty() => format!("pp. {p}"),
                _ => String::new(),
            }
        },
        get("rlterms_placeOfPublication"),
        get("publisher"),
        {
            let d = get("date");
            if d.is_empty() { get("rlterms_year") } else { d }
        },
    ] {
        if !d.is_empty() && !details.contains(&d) {
            details.push(d);
        }
    }
    let tags: Vec<String> = extra["citationData"]["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| text(&t["tagText"]))
        .filter(|t| !t.is_empty())
        .collect();
    let importance = importance_in(&tags.join(" "))
        .map(str::to_owned)
        .or_else(|| tags.first().map(|t| t.to_lowercase()));
    let doi = get("doi");
    let isbn = [
        get("identifier_isbn"),
        get("identifier_issn"),
        get("identifier_eissn"),
    ]
    .into_iter()
    .find(|s| !s.is_empty())
    .map(|s| s.split(';').next().unwrap_or_default().trim().to_owned());
    let link = [get("source"), get("urls")]
        .into_iter()
        .flat_map(|s| {
            s.split(';')
                .map(str::trim)
                .map(str::to_owned)
                .collect::<Vec<_>>()
        })
        .find(|s| s.starts_with("http"));
    // The lecturer's note to students, then the library's note.
    let mut note = [
        crate::html_to_markdown(&text(&extra["socialStatistics"]["publicNote"])),
        get("rlterms_note"),
    ]
    .into_iter()
    .map(|n| n.trim().to_owned())
    .filter(|n| !n.is_empty())
    .collect::<Vec<_>>()
    .join(" | ");
    if !chapter_of.is_empty() {
        let title = get("title");
        if !note.is_empty() {
            note.push_str(" | ");
        }
        note.push_str(&format!("Chapter \"{chapter_of}\" of {title}"));
    }
    let mut tag_line = String::new();
    if tags.len() > 1 {
        tag_line = format!("Tags: {}", tags.join(", "));
    }
    if !tag_line.is_empty() {
        if !note.is_empty() {
            note.push_str(" | ");
        }
        note.push_str(&tag_line);
    }
    Reading {
        title: if chapter_of.is_empty() {
            get("title")
        } else {
            chapter_of.clone()
        },
        kind: kind.into(),
        authors: ["creator", "rlterms_editor", "rlterms_additionalPersonName"]
            .into_iter()
            .map(get)
            .find(|a| !a.is_empty())
            .unwrap_or_default()
            .split(';')
            .map(str::trim)
            .filter(|a| !a.is_empty())
            .map(str::to_owned)
            .collect(),
        details: details.join(", "),
        importance,
        url: (!doi.is_empty())
            .then(|| format!("https://doi.org/{doi}"))
            .or(link)
            .or_else(|| page.map(str::to_owned)),
        doi: (!doi.is_empty()).then_some(doi),
        isbn,
        note,
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_lti_form() {
        let html = r#"<html><form id="search" action="/search"><input name="q"></form>
            <form action="https://x.alma.exlibrisgroup.com/lti/v3/login/I/C" method="post" target="f">
            <input type="hidden" name="iss" value="https://canvas.instructure.com">
            <input type="hidden" name="login_hint" value="abc&amp;d">
            <input type="hidden" name="lti_message_hint" value='m'>
            </form></html>"#;
        let f = lti_form(html).unwrap();
        assert_eq!(
            f.action,
            "https://x.alma.exlibrisgroup.com/lti/v3/login/I/C"
        );
        assert_eq!(f.method, Method::POST);
        assert_eq!(f.fields[1], ("login_hint".into(), "abc&d".into()));
        assert!(lti_form(r#"<form action="/s"><input name="q"></form>"#).is_none());
        let again = lti_form(
            "<form method='post'><input type='hidden' name='login_hint' value='x'></input></form>",
        )
        .unwrap();
        assert_eq!(again.action, "");
    }

    #[test]
    fn maps_leganto_citations() {
        let list: Value = serde_json::from_str(
            r#"{"name":"Digital Business","sections":[{"id":"S1","name":"Lecture 1: Intro","citations":[
              {"id":"C1","type":"BK","secondaryType":"BK","dc":{"title":"Management information systems","creator":"Laudon, K. C.; Laudon, J. P.","rlterms_edition":"18th Ed.","publisher":"Pearson","date":"2025","identifier_isbn":"9781292450223; 1292450223","rlterms_note":"Includes ebook access."}},
              {"id":"C2","type":"CR","secondaryType":"CR","dc":{"title":"Sunk costs","creator":"Ettlie, J. E.","rlterms_journalTitle":"Management Science","volume":"30","rlterms_issue":"6","start_page":"682","end_page":"695","doi":"10.1287/mnsc.30.6.682"}}
            ]}]}"#,
        )
        .unwrap();
        let extra: Value = serde_json::from_str(
            r#"{"citations":{"C1":{"citationData":{"tags":[{"tagText":"Background"},{"tagText":"Essential "}]},"socialStatistics":{"publicNote":"Chapter 1 is essential"}},
                              "C2":{"citationData":{"tags":[{"tagText":"Recommended"}]}}}}"#,
        )
        .unwrap();
        let s = sections_from(&list, &extra, Some("https://x/leganto/nui/lists/1"));
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].heading, "Lecture 1: Intro");
        assert_eq!(s[0].source, "leganto");
        let book = &s[0].items[0];
        assert_eq!(book.kind, "book");
        assert_eq!(book.importance.as_deref(), Some("essential"));
        assert_eq!(book.authors, ["Laudon, K. C.", "Laudon, J. P."]);
        assert_eq!(book.isbn.as_deref(), Some("9781292450223"));
        assert!(book.details.contains("18th ed."), "{}", book.details);
        assert!(
            book.note
                .starts_with("Chapter 1 is essential | Includes ebook access."),
            "{}",
            book.note
        );
        assert!(book.note.contains("Background"));
        assert_eq!(book.url.as_deref(), Some("https://x/leganto/nui/lists/1"));
        let art = &s[0].items[1];
        assert_eq!(art.kind, "article");
        assert_eq!(art.importance.as_deref(), Some("recommended"));
        assert_eq!(
            art.url.as_deref(),
            Some("https://doi.org/10.1287/mnsc.30.6.682")
        );
        assert!(
            art.details
                .starts_with("Management Science, vol. 30, issue 6, pp. 682-695")
        );
    }
}
