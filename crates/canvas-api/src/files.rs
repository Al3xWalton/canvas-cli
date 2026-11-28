//! Course files and folders, the user's own files, and links to files.
//!
//! Many universities refuse the Files list to students
//! with a 403 while still linking files from modules and from inside
//! pages. Callers should treat [`Client::files`] as optional and fall back
//! to module `File` items and page links, fetching each with
//! [`Client::file`].
//!
//! A file link is `/courses/<course>/files/<file>` on the Canvas site, with
//! any tail (`?wrap=1`, `/preview`, `/download?download_frd=1`), the
//! `/api/v1` form, or the Files tab's `/courses/<course>/files?preview=<file>`.
//! A link into a user's own files is `/users/<user>/files/<file>`, with the
//! same tails. Either may be absolute or relative to the site. Only the ids
//! are read from a link; nothing is ever fetched from it.

use std::sync::LazyLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};

use canvas_core::{Error, Result};

use crate::Client;

/// The `href` of each `<a>` tag, quoted either way or bare. Only anchors
/// count: images (`<img src>`) and embeds show in the page, they are not
/// files it links to. Earlier attributes are skipped whole, so a `>` or an
/// ` href=` inside a quoted value (`title="Week 1 > Slides"`) is not
/// mistaken for the end of the tag or the link.
static ANCHOR_HREF: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r#"(?is)<a\b(?:[^>"']|"[^"]*"|'[^']*')*?\shref\s*=\s*(?:"([^"]*)"|'([^']*)'|([^\s>"']+))"#,
    )
    .ok()
});

/// A file's metadata. `url` is a short-lived download link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasFile {
    /// File id.
    pub id: u64,
    /// Name shown in Canvas.
    #[serde(default)]
    pub display_name: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Last change in Canvas; used to skip unchanged files.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Folder the file lives in.
    #[serde(default)]
    pub folder_id: Option<u64>,
    /// MIME type.
    #[serde(default, rename = "content-type")]
    pub content_type: Option<String>,
    /// Download link (includes a verifier). Not printed in text output.
    #[serde(default)]
    pub url: String,
    /// True if the file exists but is locked for this user.
    #[serde(default)]
    pub locked_for_user: bool,
    /// Why it is locked, e.g. until a date. Canvas text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock_explanation: Option<String>,
}

/// A folder of a course or a user, used to place files on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// Folder id.
    pub id: u64,
    /// Path from the top folder, e.g. `course files/Lectures/Week 1`, or
    /// `my files/Notes` in a user's own files.
    #[serde(default)]
    pub full_name: String,
    /// What holds the folder: `Course`, `User` or `Group`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_type: Option<String>,
    /// The id of the course, user or group holding it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_id: Option<u64>,
}

/// A file in a user's own files, as a `/users/<user>/files/<file>` link
/// names it. The user id is only what the link says; who really holds the
/// file is its folder's [`Folder::context_id`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserFile {
    /// The user the link names.
    pub user_id: u64,
    /// File id.
    pub file_id: u64,
}

/// What a file argument (an id or a link) names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileTarget {
    /// A file of a course.
    Course {
        /// Course id.
        course_id: u64,
        /// File id.
        file_id: u64,
    },
    /// A file in a user's own files.
    User(UserFile),
}

/// The ids a file link names.
enum Link {
    Course(u64, u64),
    User(UserFile),
}

impl Client {
    /// Lists every file in a course.
    ///
    /// # Errors
    /// As for [`Client::get_all`]; [`canvas_core::Error::Forbidden`] when
    /// the course hides its Files list from the user.
    pub async fn files(&self, course_id: u64) -> Result<Vec<CanvasFile>> {
        self.get_all(&format!("/api/v1/courses/{course_id}/files?per_page=100"))
            .await
    }

    /// Fetches one file's metadata.
    ///
    /// # Errors
    /// As for [`Client::get`].
    pub async fn file(&self, course_id: u64, file_id: u64) -> Result<CanvasFile> {
        self.get(&format!("/api/v1/courses/{course_id}/files/{file_id}"))
            .await
    }

    /// Fetches one file's metadata by its id alone, from
    /// `GET /api/v1/files/<id>`, wherever it lives. Used for the user's own
    /// files, which belong to no course.
    ///
    /// # Errors
    /// As for [`Client::get`].
    pub async fn file_by_id(&self, file_id: u64) -> Result<CanvasFile> {
        self.get(&format!("/api/v1/files/{file_id}")).await
    }

    /// Lists a course's folders.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    pub async fn folders(&self, course_id: u64) -> Result<Vec<Folder>> {
        self.get_all(&format!("/api/v1/courses/{course_id}/folders?per_page=100"))
            .await
    }

    /// Fetches one folder, with what holds it.
    ///
    /// # Errors
    /// As for [`Client::get`].
    pub async fn folder(&self, folder_id: u64) -> Result<Folder> {
        self.get(&format!("/api/v1/folders/{folder_id}")).await
    }

    /// Reads `(course id, file id)` from a file link on this Canvas site,
    /// e.g. `https://canvas.example.edu/courses/1/files/42?wrap=1`,
    /// `/courses/1/files/42/download` or `canvas.example.edu/courses/1/files/42`,
    /// optionally in `<>` or quotes as pasted. `None` for anything else,
    /// including a file link on another site.
    pub fn file_link(&self, text: &str) -> Option<(u64, u64)> {
        match self.any_file_link(text)? {
            Link::Course(course, file) => Some((course, file)),
            Link::User(_) => None,
        }
    }

    /// Reads a course file link, or a `/users/<user>/files/<file>` link
    /// into a user's own files, on this Canvas site.
    fn any_file_link(&self, text: &str) -> Option<Link> {
        let text = text.trim().trim_matches(['<', '>', '"', '\'']).trim();
        file_ids(&self.site_url(text)?)
    }

    /// What a file argument names: a file id (`12345678`), which needs
    /// `course_id`; a file link in that course (or in any course when
    /// `course_id` is `None`); or a link into a user's own files, which
    /// belong to no course.
    ///
    /// # Errors
    /// [`Error::Input`] if `text` is none of these, is an id without a
    /// course, or links a file in a course other than `course_id`.
    pub fn file_target(&self, course_id: Option<u64>, text: &str) -> Result<FileTarget> {
        let text = text.trim();
        if let Ok(file_id) = text.parse::<u64>() {
            return match course_id {
                Some(course_id) => Ok(FileTarget::Course { course_id, file_id }),
                None => Err(Error::Input(format!(
                    "`{text}` is a file id, which needs its course; give the course too, \
                     or a file link instead"
                ))),
            };
        }
        match self.any_file_link(text) {
            Some(Link::Course(course, file_id)) if course_id.is_none_or(|c| c == course) => {
                Ok(FileTarget::Course {
                    course_id: course,
                    file_id,
                })
            }
            Some(Link::Course(course, _)) => Err(Error::Input(format!(
                "`{text}` is a file in course {course}, not course {}",
                course_id.unwrap_or_default()
            ))),
            Some(Link::User(file)) => Ok(FileTarget::User(file)),
            None => Err(Error::Input(format!(
                "`{text}` is not a file id or a file link on your Canvas site \
                 (…/courses/<id>/files/<id> or …/users/<id>/files/<id>)"
            ))),
        }
    }

    /// The files of course `course_id` that an HTML page body links to
    /// with `<a href>`, in order, each once. Images and embeds are not
    /// links, and links to other courses or other sites are left out.
    pub fn linked_files(&self, course_id: u64, html: &str) -> Vec<u64> {
        let Some(re) = ANCHOR_HREF.as_ref() else {
            return Vec::new();
        };
        let mut out: Vec<u64> = Vec::new();
        for caps in re.captures_iter(html) {
            let href = caps
                .get(1)
                .or_else(|| caps.get(2))
                .or_else(|| caps.get(3))
                .map_or("", |m| m.as_str())
                .replace("&amp;", "&")
                .replace("&#38;", "&")
                .replace("&#x26;", "&");
            if let Some((course, file)) = self.file_link(&href)
                && course == course_id
                && !out.contains(&file)
            {
                out.push(file);
            }
        }
        out
    }

    /// `text` as a URL on this Canvas site: absolute, relative to the site
    /// (`/courses/...`), or the site's host without a scheme. `None` if it
    /// is none of these, or is on another host or port.
    fn site_url(&self, text: &str) -> Option<url::Url> {
        let lower = text.to_ascii_lowercase();
        let host = self.base.host_str()?.to_ascii_lowercase();
        let url = if lower.starts_with("https://") || lower.starts_with("http://") {
            url::Url::parse(text).ok()?
        } else if text.starts_with('/') && !text.starts_with("//") {
            self.base.join(text).ok()?
        } else if lower.starts_with(&host) {
            url::Url::parse(&format!("{}://{text}", self.base.scheme())).ok()?
        } else {
            return None;
        };
        // `port()` is `None` for a scheme's default port, so `https://host`,
        // `https://host:443` and `http://host` all match a default-port site.
        let same_site = url.host_str() == self.base.host_str() && url.port() == self.base.port();
        same_site.then_some(url)
    }
}

/// The course (or user) and file ids in a file link's path (and, for the
/// Files tab's preview, its query).
fn file_ids(url: &url::Url) -> Option<Link> {
    let parts: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
    let parts = match parts.as_slice() {
        ["api", "v1", rest @ ..] => rest,
        all => all,
    };
    match parts {
        ["courses", course, "files", file, ..] if is_id(file) => {
            Some(Link::Course(course.parse().ok()?, file.parse().ok()?))
        }
        ["users", user, "files", file, ..] if is_id(user) && is_id(file) => {
            Some(Link::User(UserFile {
                user_id: user.parse().ok()?,
                file_id: file.parse().ok()?,
            }))
        }
        // The Files tab previewing a file: `files?preview=42`, also inside
        // a folder (`files/folder/Week 1?preview=42`).
        ["courses", course, "files", ..] => {
            let preview = url
                .query_pairs()
                .find(|(k, _)| k == "preview")
                .map(|(_, v)| v.into_owned())?;
            if !is_id(&preview) {
                return None;
            }
            Some(Link::Course(course.parse().ok()?, preview.parse().ok()?))
        }
        _ => None,
    }
}

/// True for a non-empty run of ASCII digits: `2abc` is not file 2.
fn is_id(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::tests::{client, serve};

    async fn site() -> Client {
        let (base, _) = serve(vec![]).await;
        client(&base)
    }

    fn base_of(c: &Client) -> String {
        c.base.as_str().trim_end_matches('/').to_owned()
    }

    #[tokio::test]
    async fn every_link_shape_gives_course_and_file() {
        let c = site().await;
        let base = base_of(&c);
        let host = base.trim_start_matches("http://").to_owned();
        for link in [
            "/courses/70/files/42".to_owned(),
            "/courses/70/files/42?wrap=1".to_owned(),
            "/courses/70/files/42/download?download_frd=1".to_owned(),
            "/courses/70/files/42/preview".to_owned(),
            "/courses/70/files?preview=42".to_owned(),
            "/courses/70/files/folder/Week%201?preview=42".to_owned(),
            format!("{base}/courses/70/files/42?wrap=1"),
            format!("{base}/api/v1/courses/70/files/42"),
            format!("  <{}/courses/70/files/42>  ", base.to_uppercase()),
            format!("\"{base}/courses/70/files/42\""),
            format!("{host}/courses/70/files/42"),
        ] {
            assert_eq!(c.file_link(&link), Some((70, 42)), "{link}");
        }
        for not_a_file in [
            "https://other.example/courses/70/files/42".to_owned(),
            format!("{base}0/courses/70/files/42"),
            "//other.example/courses/70/files/42".to_owned(),
            "/courses/70/pages/intro".to_owned(),
            "/courses/70/files/2abc".to_owned(),
            "/courses/70/files?preview=x".to_owned(),
            "/users/5/files/42".to_owned(),
            "/files/42".to_owned(),
            "see /courses/70/files/42".to_owned(),
            "42".to_owned(),
        ] {
            assert_eq!(c.file_link(&not_a_file), None, "{not_a_file}");
        }
    }

    #[test]
    fn the_default_port_is_the_same_site() {
        let config = canvas_core::Config {
            canvas_url: "https://canvas.example.edu".into(),
            ..canvas_core::Config::default()
        };
        let session = canvas_auth::Session {
            captured_at: 0,
            cookies: vec![canvas_auth::Cookie {
                name: "canvas_session".into(),
                value: "x".into(),
                domain: "canvas.example.edu".into(),
                path: "/".into(),
                expires: None,
            }],
        };
        let c = Client::new(&config, &session).unwrap();
        let link = "https://canvas.example.edu:443/courses/70/files/42";
        assert_eq!(c.file_link(link), Some((70, 42)));
        let plain = "http://canvas.example.edu/courses/70/files/42";
        assert_eq!(c.file_link(plain), Some((70, 42)));
        assert_eq!(
            c.file_link("https://canvas.example.edu:8443/courses/70/files/42"),
            None
        );
    }

    #[tokio::test]
    async fn file_arguments_are_ids_or_links_in_the_course() {
        let c = site().await;
        let base = base_of(&c);
        let course = |course_id, file_id| FileTarget::Course { course_id, file_id };
        assert_eq!(
            c.file_target(Some(70), " 12345678 ").unwrap(),
            course(70, 12_345_678)
        );
        let link = format!("{base}/courses/70/files/42?wrap=1");
        assert_eq!(c.file_target(Some(70), &link).unwrap(), course(70, 42));
        // Without a course, a link names its own.
        assert_eq!(c.file_target(None, &link).unwrap(), course(70, 42));
        let err = c.file_target(Some(71), &link).unwrap_err().to_string();
        assert!(err.contains("course 70, not course 71"), "{err}");
        let err = c.file_target(None, "42").unwrap_err().to_string();
        assert!(err.contains("needs its course"), "{err}");
        assert!(c.file_target(Some(70), "slides.pdf").is_err());
        assert!(
            c.file_target(Some(70), "https://other.example/courses/70/files/42")
                .is_err()
        );
    }

    #[tokio::test]
    async fn user_file_links_name_the_user_and_file_in_any_course() {
        let c = site().await;
        let base = base_of(&c);
        let mine = FileTarget::User(UserFile {
            user_id: 9,
            file_id: 42,
        });
        for link in [
            "/users/9/files/42".to_owned(),
            "/users/9/files/42?wrap=1".to_owned(),
            "/users/9/files/42/download?download_frd=1".to_owned(),
            "/users/9/files/42/preview?verifier=v".to_owned(),
            format!("{base}/users/9/files/42"),
            format!("{base}/api/v1/users/9/files/42"),
            format!("<{base}/users/9/files/42>"),
        ] {
            // A course given alongside does not matter: these belong to none.
            for course_id in [None, Some(70)] {
                assert_eq!(c.file_target(course_id, &link).unwrap(), mine, "{link}");
            }
            // Never taken for a course file, so pages do not list them.
            assert_eq!(c.file_link(&link), None, "{link}");
        }
        for not_a_file in [
            "/users/9/files".to_owned(),
            "/users/9/files?preview=42".to_owned(),
            "/users/self/files/42".to_owned(),
            "/users/9/files/4x2".to_owned(),
            "https://other.example/users/9/files/42".to_owned(),
            "/users/9/pages/42".to_owned(),
        ] {
            assert!(c.file_target(None, &not_a_file).is_err(), "{not_a_file}");
        }
    }

    #[tokio::test]
    async fn pages_link_this_courses_files_once_by_anchor_only() {
        let c = site().await;
        let base = base_of(&c);
        let html = format!(
            r#"<p>Slides: <a href="{base}/courses/70/files/42?wrap=1" title="Week 1.pdf" data-api-endpoint="{base}/api/v1/courses/70/files/42">Week 1.pdf</a></p>
            <p>Again: <a class="x" href='/courses/70/files/42/download'>same</a></p>
            <p><img src="{base}/courses/70/files/43/preview" alt="Figure [1] chart"></p>
            <p><a href="{base}/courses/70/files/45?wrap=1&amp;verifier=v"><img src="{base}/courses/70/files/44/preview"></a></p>
            <p>Read this!<a href="{base}/courses/70/files/46">handout</a></p>
            <p>Other course: <a href="{base}/courses/71/files/47?wrap=1">x</a></p>
            <p>Other site: <a href="https://other.example/courses/70/files/48">y</a></p>
            <p>Inside another link: <a href="https://other.example/a/courses/70/files/49">z</a></p>
            <p><A HREF=/courses/70/files/50/download>bare</A></p>
            <p>Files tab: <a href="/courses/70/files?preview=51">preview</a></p>
            <p><a title="Week 1 > Slides" href="/courses/70/files/52">angle in a title</a></p>
            <p><a title=" href=/courses/70/files/53" href="/courses/70/files/54">href in a title</a></p>
            <p><a href="/courses/70/files?x=1&#38;preview=55">encoded</a></p>
            <p>Someone's own files: <a href="{base}/users/9/files/56?verifier=v">mine</a></p>"#
        );
        assert_eq!(
            c.linked_files(70, &html),
            vec![42, 45, 46, 50, 51, 52, 54, 55]
        );
    }
}
