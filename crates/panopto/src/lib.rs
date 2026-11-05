//! Panopto ReCap support: identify recordings and resolve their streams.
//!
//! A recording is identified by its **delivery ID**, a UUID found in viewer
//! and embed URLs (`.../Viewer.aspx?id=<uuid>`, `.../Embed.aspx?id=<uuid>`).
//!
//! [`Panopto::recording`] posts the ID to
//! `/Panopto/Pages/Viewer/DeliveryInfo.aspx` with the Panopto cookies from
//! `canvas auth login` (and the `csrfToken` cookie echoed as
//! `X-Csrf-Token`, as the web viewer does). The JSON answer lists:
//!
//! - `PodcastStreams`: a single composite **MP4** on a signed CDN URL. This
//!   is what a normal download should take; it needs no cookie and no
//!   ffmpeg.
//! - `Streams`: one HLS (`.m3u8`) stream per source, tagged `audio` for the
//!   camera/primary feed and `object` for screen captures.
//!
//! Panopto reports failures inside a 200 response: `ErrorCode` with
//! `LoginRedirect: true` means the session expired
//! ([`canvas_core::Error::SessionExpired`]); code 5 means the link is
//! invalid ([`canvas_core::Error::NotFound`]).
//!
//! Recordings for a course are found through the web UI's own data
//! service: [`Panopto::course_recordings`] searches folders for the course
//! code (`/Panopto/Services/Data.svc/GetFolders`), then lists each matching
//! folder's sessions (`GetSessions`).
//!
//! The protocol was learned by studying the GPL-3.0
//! `travbend/panopto-downloader` extension; no code was copied from it.

mod course;
mod download;
mod transcript;

pub use course::{RecapTargets, recap_targets};
pub use download::{DownloadOptions, Downloaded, file_stem};
pub use transcript::{Cue, Transcript, parse_srt, to_text};

use canvas_auth::Session;
use canvas_core::{Config, Error, Result};
use chrono::{DateTime, Utc};
use reqwest::header;
use serde::{Deserialize, Serialize};

/// An authenticated client for one Panopto tenant.
#[derive(Debug, Clone)]
pub struct Panopto {
    pub(crate) http: reqwest::Client,
    host: String,
    /// The live session. Panopto refreshes its auth cookie in responses;
    /// those updates land here and, with persistence on, in the keychain.
    session: std::sync::Arc<std::sync::Mutex<Session>>,
    persist: bool,
}

/// One stream of a recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Stream {
    /// `camera` (Panopto tag `audio`), `screen` (tag `object`) or the raw tag.
    pub kind: String,
    /// HLS playlist URL (signed, short-lived).
    pub url: String,
}

/// A chapter heading from Panopto's AI chapters.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Chapter {
    /// Offset from the start, in seconds.
    pub start_secs: f64,
    /// Heading text. Third-party text.
    pub title: String,
}

/// What Panopto knows about one recording.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Recording {
    /// Delivery ID.
    pub id: String,
    /// Session title, e.g. `ABC1016/L01/01 Thu Oct 29 14:35:00 GMT 2026`.
    pub title: String,
    /// Folder (usually the course), if given.
    pub folder: Option<String>,
    /// When it was recorded.
    pub start_time: Option<DateTime<Utc>>,
    /// Length in seconds.
    pub duration_secs: f64,
    /// Composite MP4 URL (signed, short-lived), if Panopto encoded one.
    pub mp4_url: Option<String>,
    /// Per-source HLS streams.
    pub streams: Vec<Stream>,
    /// Whether captions exist.
    pub has_captions: bool,
    /// Caption language codes Panopto offers (pass one to the transcript
    /// endpoint).
    pub caption_languages: Vec<i64>,
    /// AI chapter headings, if any.
    pub chapters: Vec<Chapter>,
    /// Always true: `title`, `folder` and chapter titles are third-party.
    pub untrusted_text: bool,
}

/// Reads a field Panopto may send as `null` as its default value.
fn null_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Envelope {
    #[serde(default)]
    error_code: Option<i64>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    login_redirect: bool,
    #[serde(default)]
    delivery: Option<Delivery>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct Delivery {
    #[serde(default, deserialize_with = "null_default")]
    session_name: String,
    #[serde(default, deserialize_with = "null_default")]
    session_group_long_name: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    session_start_time: Option<f64>,
    #[serde(default, deserialize_with = "null_default")]
    duration: f64,
    #[serde(default, deserialize_with = "null_default")]
    streams: Vec<RawStream>,
    #[serde(default, deserialize_with = "null_default")]
    podcast_streams: Vec<RawStream>,
    #[serde(default, deserialize_with = "null_default")]
    has_captions: bool,
    #[serde(default, deserialize_with = "null_default")]
    available_captions: Vec<RawCaption>,
    #[serde(default, rename = "AIChapters", deserialize_with = "null_default")]
    ai_chapters: Vec<RawChapter>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawStream {
    #[serde(default)]
    tag: Option<String>,
    #[serde(default)]
    stream_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawCaption {
    #[serde(default)]
    language: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawChapter {
    #[serde(default)]
    start: Option<f64>,
    #[serde(default)]
    summary: Option<String>,
}

/// Seconds between 1601-01-01 (the Windows epoch Panopto counts
/// `SessionStartTime` from) and 1970-01-01.
const WINDOWS_TO_UNIX_SECS: f64 = 11_644_473_600.0;

/// A recording as listed in a folder, before its streams are resolved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RecordingSummary {
    /// Delivery ID; pass to [`Panopto::recording`] or a download.
    pub id: String,
    /// Session title. Third-party text.
    pub title: String,
    /// Folder name. Third-party text.
    pub folder: String,
    /// When it was recorded.
    pub start_time: Option<DateTime<Utc>>,
    /// Length in seconds.
    pub duration_secs: f64,
    /// Whether captions exist.
    pub has_captions: bool,
    /// Always true: `title` and `folder` are third-party text.
    pub untrusted_text: bool,
}

/// A Panopto folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Folder {
    /// Folder ID.
    pub id: String,
    /// Folder name, e.g. `2026/27 - Example Studies - ABC1020`.
    pub name: String,
}

#[derive(Deserialize)]
struct DataEnvelope<T> {
    d: T,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ResultPage<T> {
    #[serde(default = "none")]
    results: Option<Vec<T>>,
    #[serde(default, deserialize_with = "null_default")]
    total_number: u64,
}

#[derive(Deserialize)]
struct RawFolder {
    #[serde(rename = "ID", default)]
    id: Option<String>,
    #[serde(rename = "Name", default, deserialize_with = "null_default")]
    name: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RawSession {
    #[serde(rename = "DeliveryID", default)]
    delivery_id: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    session_name: String,
    #[serde(default, deserialize_with = "null_default")]
    folder_name: String,
    #[serde(default)]
    start_time: Option<String>,
    #[serde(default, deserialize_with = "null_default")]
    duration: f64,
    #[serde(default, deserialize_with = "null_default")]
    has_captions: bool,
}

fn none<T>() -> Option<T> {
    None
}

/// Rows requested per data-service page.
const PAGE_SIZE: u64 = 100;
/// Pages fetched per listing before giving up.
const MAX_SESSION_PAGES: u64 = 50;

/// Removes HTML tags (Panopto wraps search hits in `<span class="match">`)
/// and control characters, and collapses whitespace.
fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        // Only `<` followed by a letter or `/` opens a tag, so text such as
        // `Costs < Revenue` survives.
        if c == '<'
            && chars
                .peek()
                .is_some_and(|n| n.is_ascii_alphabetic() || *n == '/')
        {
            for t in chars.by_ref() {
                if t == '>' {
                    break;
                }
            }
        } else if !c.is_control() {
            out.push(c);
        }
    }
    let out = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parses Microsoft JSON dates like `/Date(1793284500325)/`.
fn ms_date(text: &str) -> Option<DateTime<Utc>> {
    let inner = text.strip_prefix("/Date(")?.strip_suffix(")/")?;
    // `ms` may be negative; an optional `+hhmm`/`-hhmm` offset follows but
    // does not change the UTC instant.
    let (sign, rest) = match inner.strip_prefix('-') {
        Some(rest) => (-1, rest),
        None => (1, inner),
    };
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    DateTime::from_timestamp_millis(sign * digits.parse::<i64>().ok()?)
}

/// Returns the module code at the start of a Canvas course code, e.g.
/// `ABC1016` from `ABC1016 (26/27)` or `ABC1000` from
/// `ABC1000-ABC1003 (26/27)`.
///
/// # Example
/// ```
/// assert_eq!(panopto::module_code("ABC1020 (26/27)").as_deref(), Some("ABC1020"));
/// assert_eq!(panopto::module_code("AI for Learning"), None);
/// ```
pub fn module_code(course_code: &str) -> Option<String> {
    let first: String = course_code
        .trim()
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric())
        .collect();
    let letters = first
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .count();
    let digits = first.len() - letters;
    (letters >= 2 && digits >= 3).then(|| first.to_ascii_uppercase())
}

/// Extracts a delivery ID from a bare UUID or any Panopto URL with an
/// `id=` query parameter.
///
/// # Errors
/// Returns [`Error::Input`] if no UUID can be found.
///
/// # Example
/// ```
/// let id = panopto::delivery_id(
///     "https://example.hosted.panopto.com/Panopto/Pages/Viewer.aspx?id=0a1b2c3d-1111-4222-8333-444455556666",
/// ).unwrap();
/// assert_eq!(id, "0a1b2c3d-1111-4222-8333-444455556666");
/// assert!(panopto::delivery_id("not a recording").is_err());
/// ```
pub fn delivery_id(text: &str) -> Result<String> {
    let text = text.trim();
    let candidate = match url::Url::parse(text) {
        Ok(u) => u
            .query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case("id"))
            .map(|(_, v)| v.into_owned())
            .unwrap_or_default(),
        Err(_) => text.to_owned(),
    };
    if is_uuid(&candidate) {
        Ok(candidate.to_lowercase())
    } else {
        Err(Error::Input(format!(
            "`{text}` is not a ReCap recording: give a viewer URL or a delivery ID (UUID)"
        )))
    }
}

pub(crate) fn is_uuid(s: &str) -> bool {
    let groups: Vec<&str> = s.split('-').collect();
    groups.len() == 5
        && groups
            .iter()
            .zip([8, 4, 4, 4, 12])
            .all(|(g, n)| g.len() == n && g.chars().all(|c| c.is_ascii_hexdigit()))
}

impl Panopto {
    /// Builds a client for `config.panopto_host` from `session`.
    ///
    /// # Errors
    /// [`Error::SessionExpired`] if the session has no Panopto cookies.
    pub fn new(config: &Config, session: &Session) -> Result<Self> {
        let host = config.panopto_host.trim().to_ascii_lowercase();
        let valid = !host.is_empty()
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
        if !valid {
            return Err(Error::Input(format!(
                "panopto_host `{}` must be a bare host name like example.hosted.panopto.com",
                config.panopto_host
            )));
        }
        if !session.covers(&host) {
            return Err(Error::SessionExpired);
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("canvas-cli/", env!("CARGO_PKG_VERSION")))
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(std::time::Duration::from_secs(10))
            .read_timeout(std::time::Duration::from_secs(60))
            .build()
            .map_err(|e| Error::Http(e.to_string()))?;
        Ok(Self {
            http,
            host,
            session: std::sync::Arc::new(std::sync::Mutex::new(session.clone())),
            persist: false,
        })
    }

    /// Saves refreshed Panopto cookies to the keychain as they arrive, so
    /// normal use keeps the ReCap login alive. Off by default (tests).
    #[must_use]
    pub fn with_persistence(mut self, persist: bool) -> Self {
        self.persist = persist;
        self
    }

    /// Builds the `Cookie` header from the current session.
    pub(crate) fn cookie(&self) -> String {
        self.session
            .lock()
            .map(|s| s.cookie_header(&self.host).unwrap_or_default())
            .unwrap_or_default()
    }

    fn csrf(&self) -> Option<String> {
        self.session
            .lock()
            .ok()
            .and_then(|s| s.cookie_value(&self.host, "csrfToken").map(str::to_owned))
    }

    /// Takes in any `Set-Cookie` headers from a Panopto response.
    ///
    /// They always update the in-memory session for this run. With
    /// persistence on, and only for a successful response, they are also
    /// saved: the keychain is **re-read** and just these headers are
    /// replayed onto it, so a newer login made elsewhere (e.g. `canvas auth
    /// login` while the MCP server runs) is never overwritten with stale
    /// cookies. A change to `csrfToken` alone is not saved, since Panopto
    /// issues a new one on each response anyway.
    pub(crate) fn absorb(&self, headers: &reqwest::header::HeaderMap, status: reqwest::StatusCode) {
        let now = chrono::Utc::now().timestamp();
        let lines: Vec<String> = headers
            .get_all(header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok().map(str::to_owned))
            .collect();
        if lines.is_empty() {
            return;
        }
        let mut worth_saving = false;
        if let Ok(mut session) = self.session.lock() {
            for line in &lines {
                let name = line.split('=').next().unwrap_or_default().trim();
                if session.apply_set_cookie(line, &self.host, now) && name != "csrfToken" {
                    worth_saving = true;
                }
            }
        }
        if !(worth_saving && self.persist && status.is_success()) {
            return;
        }
        // Lock released above: merge into the current keychain copy.
        if let Ok(mut stored) = canvas_auth::load() {
            let mut changed = false;
            for line in &lines {
                changed |= stored.apply_set_cookie(line, &self.host, now);
            }
            if changed {
                let _ = canvas_auth::store(&stored);
            }
        }
    }

    /// Returns the tenant's base URL. Only an exact loopback host (for
    /// tests) uses plain http; everything else is https.
    pub(crate) fn base(&self) -> String {
        let name = self
            .host
            .rsplit_once(':')
            .map_or(self.host.as_str(), |(h, _)| h);
        let loopback = matches!(name, "127.0.0.1" | "localhost");
        format!(
            "{}://{}",
            if loopback { "http" } else { "https" },
            self.host
        )
    }

    /// Looks up a recording by delivery ID.
    ///
    /// # Errors
    /// [`Error::SessionExpired`] if Panopto wants a login,
    /// [`Error::NotFound`] for an invalid link, [`Error::Forbidden`] for
    /// other Panopto refusals, and [`Error::Http`] / [`Error::Decode`] for
    /// transport or format problems.
    pub async fn recording(&self, delivery_id: &str) -> Result<Recording> {
        let id = crate::delivery_id(delivery_id)?;
        let mut request = self
            .http
            .post(format!(
                "{}/Panopto/Pages/Viewer/DeliveryInfo.aspx",
                self.base()
            ))
            .header(header::COOKIE, self.cookie())
            .form(&[
                ("deliveryId", id.as_str()),
                ("isLiveNotes", "false"),
                ("refreshAuthCookie", "true"),
                ("isActiveBroadcast", "false"),
                ("isEditing", "false"),
                ("isKollectiveAgentInstalled", "false"),
                ("isEmbed", "false"),
                ("responseType", "json"),
            ]);
        if let Some(csrf) = self.csrf() {
            request = request.header("X-Csrf-Token", csrf);
        }
        let response = request
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        let status = response.status();
        self.absorb(response.headers(), status);
        if status.is_redirection() || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::SessionExpired);
        }
        if !status.is_success() {
            return Err(Error::Http(format!("ReCap DeliveryInfo returned {status}")));
        }
        let text = response
            .text()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        parse(&id, &text)
    }
}

impl Panopto {
    async fn data<B: Serialize, T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        body: &B,
    ) -> Result<T> {
        let mut request = self
            .http
            .post(format!(
                "{}/Panopto/Services/Data.svc/{method}",
                self.base()
            ))
            .header(header::COOKIE, self.cookie())
            .json(body);
        if let Some(csrf) = self.csrf() {
            request = request.header("X-Csrf-Token", csrf);
        }
        let response = request
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        let status = response.status();
        self.absorb(response.headers(), status);
        if status.is_redirection() || status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::SessionExpired);
        }
        if status == reqwest::StatusCode::FORBIDDEN {
            return Err(Error::Forbidden(format!("ReCap {method}")));
        }
        if !status.is_success() {
            return Err(Error::Http(format!("ReCap {method} returned {status}")));
        }
        let text = response
            .text()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        // A login page or a null payload means the session is gone.
        if text.trim_start().starts_with('<') {
            return Err(Error::SessionExpired);
        }
        let value: serde_json::Value = serde_json::from_str(&text).map_err(|e| Error::Decode {
            what: format!("ReCap {method}"),
            detail: e.to_string(),
        })?;
        if value.get("d").is_none_or(serde_json::Value::is_null) {
            return Err(Error::SessionExpired);
        }
        serde_json::from_value::<DataEnvelope<T>>(value)
            .map(|e| e.d)
            .map_err(|e| Error::Decode {
                what: format!("ReCap {method}"),
                detail: e.to_string(),
            })
    }

    /// Confirms the Panopto session is signed in.
    ///
    /// The data service answers a signed-out user with empty results, not
    /// an error, so every listing checks this first. It asks DeliveryInfo
    /// about the all-zero ID: signed-out users get code 2, signed-in users
    /// get code 5 ("link invalid").
    ///
    /// # Errors
    /// [`Error::SessionExpired`] if signed out; transport errors otherwise.
    pub async fn check_session(&self) -> Result<()> {
        match self.recording("00000000-0000-0000-0000-000000000000").await {
            Ok(_) | Err(Error::NotFound(_)) => Ok(()),
            Err(e) => Err(e),
        }
    }

    /// Searches folders the user can see by name.
    ///
    /// # Errors
    /// As for [`Panopto::recording`].
    pub async fn folders(&self, query: &str) -> Result<Vec<Folder>> {
        let mut out = Vec::new();
        for page in 0..MAX_SESSION_PAGES {
            let body = serde_json::json!({ "queryParameters": {
                "query": query, "sortColumn": 1, "sortAscending": true,
                "maxResults": PAGE_SIZE, "page": page
            }});
            let result: ResultPage<RawFolder> = self.data("GetFolders", &body).await?;
            let rows = result.results.unwrap_or_default();
            let raw = rows.len() as u64;
            out.extend(rows.into_iter().filter_map(|f| {
                Some(Folder {
                    id: f.id.filter(|id| is_uuid(id))?,
                    name: plain(&f.name),
                })
            }));
            if raw < PAGE_SIZE {
                return Ok(out);
            }
        }
        Err(Error::Http(format!(
            "ReCap folder search for `{query}` has too many pages"
        )))
    }

    /// Lists every recording in a folder, newest first.
    ///
    /// # Errors
    /// As for [`Panopto::recording`]; [`Error::Input`] for a bad folder ID.
    pub async fn folder_recordings(&self, folder_id: &str) -> Result<Vec<RecordingSummary>> {
        if !is_uuid(folder_id) {
            return Err(Error::Input(format!(
                "`{folder_id}` is not a ReCap folder ID"
            )));
        }
        let mut out = Vec::new();
        let mut seen = 0u64;
        for page in 0..MAX_SESSION_PAGES {
            let body = serde_json::json!({ "queryParameters": {
                "query": null, "sortColumn": 1, "sortAscending": false, "maxResults": PAGE_SIZE,
                "page": page, "folderID": folder_id, "getFolderData": false,
                "includeArchived": true, "includePlaylists": false
            }});
            let result: ResultPage<RawSession> = self.data("GetSessions", &body).await?;
            let rows = result.results.unwrap_or_default();
            let raw = rows.len() as u64;
            seen += raw;
            out.extend(rows.into_iter().filter_map(|s| {
                Some(RecordingSummary {
                    id: s.delivery_id.filter(|id| is_uuid(id))?.to_lowercase(),
                    title: plain(&s.session_name),
                    folder: plain(&s.folder_name),
                    start_time: s.start_time.as_deref().and_then(ms_date),
                    duration_secs: s.duration,
                    has_captions: s.has_captions,
                    untrusted_text: true,
                })
            }));
            // Count raw rows, not kept ones, and stop on a short page, so a
            // missing total or a filtered row never cuts or extends paging.
            if raw < PAGE_SIZE || (result.total_number > 0 && seen >= result.total_number) {
                return Ok(out);
            }
        }
        Err(Error::Http(format!(
            "ReCap folder {folder_id} has more than {} pages",
            MAX_SESSION_PAGES
        )))
    }

    /// Lists every recording in the folders named for `module_code`
    /// (e.g. `ABC1016`), newest first.
    ///
    /// A folder matches when its name contains the code as a whole word
    /// (so `ABC1016` never matches `ABC10160`). If `term` (e.g. `2026/27`)
    /// matches some folders, only those are used; if it matches none, all
    /// years are used and [`CourseRecordings::warning`] says so.
    ///
    /// # Errors
    /// [`Error::SessionExpired`] if the Panopto session is signed out
    /// (checked first, since the data service hides that); otherwise as
    /// for [`Panopto::folder_recordings`], and [`Error::NotFound`] if no
    /// folder matches.
    pub async fn course_recordings(
        &self,
        module_code: &str,
        term: Option<&str>,
    ) -> Result<CourseRecordings> {
        self.check_session().await?;
        let code = module_code.to_ascii_uppercase();
        let mut folders: Vec<Folder> = self
            .folders(&code)
            .await?
            .into_iter()
            .filter(|f| contains_word(&f.name.to_ascii_uppercase(), &code))
            .collect();
        let mut warning = None;
        match term {
            Some(term) if folders.iter().any(|f| f.name.contains(term)) => {
                folders.retain(|f| f.name.contains(term));
            }
            Some(term) if !folders.is_empty() => {
                warning = Some(format!("no {code} folder for {term}; showing every year"));
            }
            _ => {}
        }
        if folders.is_empty() {
            return Err(Error::NotFound(format!("no ReCap folder for {code}")));
        }
        let mut recordings = Vec::new();
        for folder in &folders {
            recordings.extend(self.folder_recordings(&folder.id).await?);
        }
        recordings.sort_by(|a, b| {
            b.start_time
                .cmp(&a.start_time)
                .then_with(|| a.id.cmp(&b.id))
        });
        recordings.dedup_by(|a, b| a.id == b.id);
        Ok(CourseRecordings {
            folders,
            recordings,
            warning,
        })
    }
}

/// The result of [`Panopto::course_recordings`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CourseRecordings {
    /// Folders the recordings came from.
    pub folders: Vec<Folder>,
    /// Recordings, newest first.
    pub recordings: Vec<RecordingSummary>,
    /// Set when the result is broader than asked, e.g. every year.
    pub warning: Option<String>,
}

/// True if `needle` occurs in `haystack` with no letter or digit directly
/// before or after it.
fn contains_word(haystack: &str, needle: &str) -> bool {
    haystack.match_indices(needle).any(|(i, _)| {
        let before = haystack[..i].chars().next_back();
        let after = haystack[i + needle.len()..].chars().next();
        !before.is_some_and(|c| c.is_ascii_alphanumeric())
            && !after.is_some_and(|c| c.is_ascii_alphanumeric())
    })
}

fn parse(id: &str, text: &str) -> Result<Recording> {
    let envelope: Envelope = serde_json::from_str(text).map_err(|e| Error::Decode {
        what: "ReCap DeliveryInfo".into(),
        detail: e.to_string(),
    })?;
    if let Some(code) = envelope.error_code.filter(|c| *c != 0) {
        // Code 2 is "Unauthorized access": a login problem whatever the flag.
        if envelope.login_redirect || code == 2 {
            return Err(Error::SessionExpired);
        }
        let message = envelope
            .error_message
            .unwrap_or_default()
            .split("<br")
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|c| !c.is_control())
            .collect::<String>()
            .trim()
            .to_owned();
        return Err(match code {
            5 => Error::NotFound(format!("ReCap recording {id}: {message}")),
            _ => Error::Forbidden(format!("ReCap recording {id}: {message} (code {code})")),
        });
    }
    let d = envelope.delivery.ok_or_else(|| Error::Decode {
        what: "ReCap DeliveryInfo".into(),
        detail: "no Delivery and no error".into(),
    })?;
    let streams = d
        .streams
        .into_iter()
        .filter_map(|s| {
            let url = s.stream_url.filter(|u| !u.is_empty())?;
            let kind = match s.tag.as_deref() {
                Some("audio") => "camera".to_owned(),
                Some("object") => "screen".to_owned(),
                Some(other) if !other.is_empty() => other.to_owned(),
                _ => "primary".to_owned(),
            };
            Some(Stream { kind, url })
        })
        .collect();
    Ok(Recording {
        id: id.to_owned(),
        title: d.session_name.trim().to_owned(),
        folder: d.session_group_long_name,
        start_time: d
            .session_start_time
            .and_then(|secs| DateTime::from_timestamp((secs - WINDOWS_TO_UNIX_SECS) as i64, 0)),
        duration_secs: d.duration,
        mp4_url: d
            .podcast_streams
            .into_iter()
            .find_map(|s| s.stream_url.filter(|u| !u.is_empty())),
        streams,
        has_captions: d.has_captions,
        caption_languages: d
            .available_captions
            .into_iter()
            .filter_map(|c| c.language)
            .collect(),
        chapters: d
            .ai_chapters
            .into_iter()
            .filter_map(|c| {
                Some(Chapter {
                    start_secs: c.start.unwrap_or(0.0),
                    title: c.summary?,
                })
            })
            .collect(),
        untrusted_text: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: &str = "0a1b2c3d-1111-4222-8333-444455556666";

    #[test]
    fn plain_strips_highlight_tags() {
        assert_eq!(
            plain(r#"2026/27 - Skills - <span class="match">ABC</span>1016 "#),
            "2026/27 - Skills - ABC1016"
        );
    }

    #[test]
    fn plain_keeps_lone_angle_brackets_and_decodes_entities() {
        assert_eq!(
            plain("Costs < Revenue &amp; Profit"),
            "Costs < Revenue & Profit"
        );
        assert_eq!(plain("a <b>bold</b> c"), "a bold c");
    }

    #[test]
    fn codes_match_as_whole_words() {
        assert!(contains_word("2026/27 - SKILLS - ABC1016", "ABC1016"));
        assert!(!contains_word("2026/27 - X - ABC10160", "ABC1016"));
        assert!(!contains_word("XABC1016", "ABC1016"));
    }

    #[test]
    fn ms_dates_parse() {
        assert!(ms_date("/Date(1793284500325-0500)/").is_some());
        assert_eq!(ms_date("/Date(-1000)/").unwrap().timestamp(), -1);
        assert_eq!(
            ms_date("/Date(1793284500325)/").unwrap().to_rfc3339(),
            "2026-10-29T14:35:00.325+00:00"
        );
        assert_eq!(
            ms_date("/Date(1793284500325+0000)/").unwrap().timestamp(),
            1_793_284_500
        );
        assert!(ms_date("2026-10-29").is_none());
    }

    #[test]
    fn module_codes_come_from_canvas_codes() {
        assert_eq!(
            module_code("ABC1000-ABC1003 (26/27)").as_deref(),
            Some("ABC1000")
        );
        assert_eq!(
            module_code("ABC1997-ABC2997-ABC3997 (26/27)").as_deref(),
            Some("ABC1997")
        );
        assert_eq!(module_code("2026-Student Induction"), None);
    }

    #[test]
    fn session_rows_parse_with_nulls() {
        let page: ResultPage<RawSession> = serde_json::from_str(
            r#"{"Results":[{"DeliveryID":"0A1B2C3D-1111-4222-8333-444455556666","SessionName":"<span>L1</span>","FolderName":null,"StartTime":"/Date(1793284500325)/","Duration":null,"HasCaptions":true},{"DeliveryID":"bad"}],"TotalNumber":2}"#,
        )
        .unwrap();
        assert_eq!(page.results.unwrap().len(), 2);
        assert_eq!(page.total_number, 2);
    }

    #[test]
    fn delivery_id_accepts_urls_and_uuids() {
        assert_eq!(delivery_id(ID).unwrap(), ID);
        assert_eq!(delivery_id(&ID.to_uppercase()).unwrap(), ID);
        let embed = format!(
            "https://example.hosted.panopto.com/Panopto/Pages/Embed.aspx?autoplay=false&id={ID}"
        );
        assert_eq!(delivery_id(&embed).unwrap(), ID);
        for bad in [
            "",
            "0a1b2c3d",
            "https://x/Viewer.aspx",
            "../../etc",
            &format!("{ID}/../x"),
        ] {
            assert!(delivery_id(bad).is_err(), "{bad}");
        }
    }

    fn client(host: &str) -> Result<Panopto> {
        let config = Config {
            panopto_host: host.into(),
            ..Config::default()
        };
        let session = Session {
            captured_at: 0,
            cookies: vec![canvas_auth::Cookie {
                name: ".ASPXAUTH".into(),
                value: "x".into(),
                domain: ".panopto.com".into(),
                path: "/".into(),
                expires: None,
            }],
        };
        Panopto::new(&config, &session)
    }

    #[test]
    fn host_must_be_bare_and_only_exact_loopback_is_http() {
        for bad in [
            "evil.com/x.example.hosted.panopto.com",
            "a@b.panopto.com",
            "x.panopto.com?y",
            "x .panopto.com",
        ] {
            assert!(matches!(client(bad), Err(Error::Input(_))), "{bad}");
        }
        assert!(
            client("localhost.cloud.panopto.com")
                .unwrap()
                .base()
                .starts_with("https://")
        );
        assert!(
            client("127.0.0.1.panopto.com")
                .unwrap()
                .base()
                .starts_with("https://")
        );
        assert!(
            client("example.hosted.panopto.com")
                .unwrap()
                .base()
                .starts_with("https://")
        );
    }

    #[test]
    fn code_two_is_expired_even_without_the_flag_and_zero_is_success() {
        let body = r#"{"ErrorCode":2,"ErrorMessage":"Unauthorized access.","LoginRedirect":false}"#;
        assert!(matches!(parse(ID, body), Err(Error::SessionExpired)));
        let ok = r#"{"ErrorCode":0,"Delivery":{"SessionName":"x"}}"#;
        assert_eq!(parse(ID, ok).unwrap().title, "x");
    }

    #[test]
    fn nulls_do_not_break_parsing() {
        let body = r#"{"Delivery":{"SessionName":null,"Duration":null,"Streams":null,"PodcastStreams":null,
            "AvailableCaptions":[{"Language":null},{"Language":1}],"AIChapters":null,"HasCaptions":null}}"#;
        let r = parse(ID, body).unwrap();
        assert_eq!(
            (r.title.as_str(), r.caption_languages.as_slice()),
            ("", &[1][..])
        );
    }

    #[test]
    fn login_redirect_means_session_expired() {
        let body = r#"{"ErrorCode":2,"ErrorMessage":"Unauthorized access.<br><a>x</a>","LoginRedirect":true}"#;
        assert!(matches!(parse(ID, body), Err(Error::SessionExpired)));
    }

    #[test]
    fn invalid_link_is_not_found_with_clean_message() {
        let body = r#"{"ErrorCode":5,"ErrorMessage":"This session link is invalid.<br><a>x</a>","LoginRedirect":false}"#;
        let err = parse(ID, body).unwrap_err().to_string();
        assert!(
            err.contains("This session link is invalid.") && !err.contains("<a>"),
            "{err}"
        );
    }

    #[test]
    fn parses_streams_mp4_and_captions() {
        let body = r#"{"Delivery":{
            "SessionName":" ABC1016/L01/01 ","SessionGroupLongName":"ABC1016","SessionStartTime":13437758100.3,
            "Duration":3299.98,"HasCaptions":true,"AvailableCaptions":[{"Language":1}],
            "Streams":[{"Tag":"audio","StreamUrl":"https://cdn/a.m3u8"},{"Tag":"object","StreamUrl":"https://cdn/s.m3u8"},{"Tag":"object","StreamUrl":null}],
            "PodcastStreams":[{"Tag":null,"StreamUrl":"https://cdn/p.mp4"}],
            "AIChapters":[{"Start":56.84,"End":529.5,"Summary":"Introduction"}]}}"#;
        let r = parse(ID, body).unwrap();
        assert_eq!(r.title, "ABC1016/L01/01");
        assert_eq!(r.mp4_url.as_deref(), Some("https://cdn/p.mp4"));
        assert_eq!(
            r.streams
                .iter()
                .map(|s| s.kind.as_str())
                .collect::<Vec<_>>(),
            ["camera", "screen"]
        );
        assert_eq!(r.caption_languages, [1]);
        assert_eq!(
            r.start_time.unwrap().to_rfc3339(),
            "2026-10-29T14:35:00+00:00"
        );
        assert_eq!(r.chapters[0].title, "Introduction");
        assert!(r.untrusted_text);
    }
}
