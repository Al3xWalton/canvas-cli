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
//! The protocol was learned by studying the GPL-3.0
//! `travbend/panopto-downloader` extension; no code was copied from it.

use canvas_auth::Session;
use canvas_core::{Config, Error, Result};
use chrono::{DateTime, Utc};
use reqwest::header;
use serde::{Deserialize, Serialize};

/// An authenticated client for one Panopto tenant.
#[derive(Debug, Clone)]
pub struct Panopto {
    http: reqwest::Client,
    host: String,
    cookie: String,
    csrf: Option<String>,
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

fn is_uuid(s: &str) -> bool {
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
        let cookie = session.cookie_header(&host).ok_or(Error::SessionExpired)?;
        let csrf = session.cookie_value(&host, "csrfToken").map(str::to_owned);
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
            cookie,
            csrf,
        })
    }

    /// Returns the tenant's base URL. Only an exact loopback host (for
    /// tests) uses plain http; everything else is https.
    fn base(&self) -> String {
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
            .header(header::COOKIE, &self.cookie)
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
        if let Some(csrf) = &self.csrf {
            request = request.header("X-Csrf-Token", csrf);
        }
        let response = request
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        let status = response.status();
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
