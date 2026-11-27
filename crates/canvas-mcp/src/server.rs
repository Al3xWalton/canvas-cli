//! The MCP tool definitions.
//!
//! Each tool is a thin wrapper over the same library call the matching CLI
//! command makes. Structured results are JSON objects: MCP requires
//! `structuredContent` to be an object and strict clients reject anything
//! else, so a list comes back under a key named for its contents
//! (`{"courses": [...]}`). Failures become MCP tool errors (not protocol
//! errors) so the agent reads the message, including the
//! `canvas auth login` instruction when the session has expired.
//!
//! No tool takes a file path. Everything written goes under the configured
//! `download_root`, in the same layout the CLI uses, so tool arguments can
//! never choose where files land.

use canvas_api::{Client, User};
use canvas_core::{Config, Error};
use panopto::Panopto;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, ContentBlock, Implementation, ServerCapabilities, ServerConfig};
use rmcp::{schemars, tool, tool_handler, tool_router};
use serde::Deserialize;

/// The agent guide, sent as `instructions` and by the `usage` tool.
pub const GUIDE: &str = include_str!("guide.md");

/// The prompt a user pastes to update, shown in the README.
pub const UPDATE_PROMPT: &str = "Update the unofficial Canvas CLI and its MCP server to the newest version by following https://github.com/Al3xWalton/canvas-cli/blob/main/UPDATE.md";

/// The update steps, built into every version so an agent can update
/// even if it cannot reach GitHub's web pages.
pub const UPDATE_STEPS: &str = include_str!("../../../UPDATE.md");

/// Most recordings one download or transcript call may fetch.
const MAX_LATEST: usize = 10;

/// Why ReCap tools cannot run.
#[derive(Debug, Clone)]
pub enum RecapUnavailable {
    /// No Panopto cookies: the user must run `canvas auth login`.
    SignedOut,
    /// The config is wrong (e.g. a bad `panopto_host`).
    Misconfigured(String),
}

/// The Canvas CLI MCP server.
#[derive(Debug, Clone)]
pub struct CanvasServer {
    config: Config,
    client: Client,
    recap: Result<Panopto, RecapUnavailable>,
    user: User,
}

/// A course: its id, code (e.g. `ABC1020`) or part of its name.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct CourseArgs {
    /// Course id, code (e.g. `ABC1020`) or part of the name.
    pub course: String,
}

/// An optional course filter plus a time window.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct WindowArgs {
    /// Window such as `7d`, `48h`, `2w`. Each tool has its own default.
    #[serde(default)]
    pub within: Option<String>,
    /// Only this course: id, code or part of the name.
    #[serde(default)]
    pub course: Option<String>,
}

/// An optional course filter.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct OptionalCourseArgs {
    /// Only this course: id, code or part of the name.
    #[serde(default)]
    pub course: Option<String>,
}

/// One page of a course.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct PageArgs {
    /// Course id, code or part of the name.
    pub course: String,
    /// Page slug: `url` from `list_pages` or `page_url` from `list_modules`.
    pub page: String,
}

/// One discussion topic.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DiscussionArgs {
    /// The discussion: its Canvas link, its `id` from `list_discussions`,
    /// or part of its title.
    pub discussion: String,
    /// Course id, code or part of the name. Needed unless `discussion` is
    /// a link.
    #[serde(default)]
    pub course: Option<String>,
}

/// A course's pages, or one of them.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SavePagesArgs {
    /// Course id, code or part of the name.
    pub course: String,
    /// Only this page slug (default: every page).
    #[serde(default)]
    pub page: Option<String>,
}

/// A course's readings, optionally one lecture's.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct ReadingsArgs {
    /// Course id, code or part of the name.
    pub course: String,
    /// Only lectures, weeks or sections whose name contains this, e.g.
    /// `week 5` or `seminar 2` (default: all).
    #[serde(default)]
    pub lecture: Option<String>,
}

/// One ReCap recording.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RecordingArgs {
    /// Viewer URL or delivery ID (from `list_recordings`).
    pub recording: String,
}

/// One recording's transcript.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct TranscriptArgs {
    /// Viewer URL or delivery ID (from `list_recordings`).
    pub recording: String,
    /// Caption language code from `recording_info`, as text (default: the
    /// first).
    #[serde(default)]
    pub language: Option<String>,
}

/// A recording, or a course's newest recordings.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct RecapTargetArgs {
    /// A viewer URL or delivery ID, or a course id, code or name.
    pub target: String,
    /// For a course, how many of the newest recordings (default 1, max 10).
    #[serde(default)]
    pub latest: Option<usize>,
    /// Caption language: a code from `recording_info`, or `all` (default:
    /// the first).
    #[serde(default)]
    pub language: Option<String>,
}

/// Download options.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DownloadArgs {
    /// A viewer URL or delivery ID, or a course id, code or name.
    pub target: String,
    /// For a course, how many of the newest recordings (default 1, max 10).
    #[serde(default)]
    pub latest: Option<usize>,
    /// Also save separate camera and screen streams (needs ffmpeg; slow).
    #[serde(default)]
    pub streams: bool,
}

/// Digest options.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct DigestArgs {
    /// `last` (default: since the previous `last` digest) or a window like `2d`.
    #[serde(default)]
    pub since: Option<String>,
    /// Also list unsubmitted work due within this window (default `7d`).
    #[serde(default)]
    pub upcoming: Option<String>,
    /// Check course files (default true; slower).
    #[serde(default)]
    pub files: Option<bool>,
    /// Check ReCap recordings (default true).
    #[serde(default)]
    pub recordings: Option<bool>,
}

/// Turns any serialisable value into a structured tool result.
///
/// `structuredContent` must be a JSON object, so anything else (a list, a
/// number, a string) is wrapped as `{"result": value}`. That is only a
/// safety net: tools that return lists name their key with [`listed`].
fn ok(value: impl serde::Serialize) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(v @ serde_json::Value::Object(_)) => CallToolResult::structured(v),
        Ok(v) => {
            let mut wrapped = serde_json::Map::new();
            wrapped.insert("result".to_owned(), v);
            CallToolResult::structured(wrapped.into())
        }
        Err(e) => fail(e),
    }
}

/// Unwraps a library list into a tool result as `{key: [...]}`, where
/// `key` names what the list holds (`courses`, `events`, ...).
fn listed<T: serde::Serialize>(key: &str, result: canvas_core::Result<Vec<T>>) -> CallToolResult {
    done(result.map(|items| std::collections::BTreeMap::from([(key, items)])))
}

/// Turns an error into a tool error the agent can read.
fn fail(e: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::error(vec![ContentBlock::text(e.to_string())])
}

/// Unwraps a library result into a tool result.
fn done<T: serde::Serialize>(result: canvas_core::Result<T>) -> CallToolResult {
    match result {
        Ok(v) => ok(v),
        Err(e) => fail(e),
    }
}

/// Parses a window, falling back to `default`.
fn window(text: Option<&str>, default: &str) -> Result<chrono::Duration, CallToolResult> {
    canvas_api::parse_window(text.unwrap_or(default)).map_err(fail)
}

impl CanvasServer {
    fn recap(&self) -> canvas_core::Result<&Panopto> {
        match &self.recap {
            Ok(r) => Ok(r),
            Err(RecapUnavailable::SignedOut) => Err(Error::SessionExpired),
            Err(RecapUnavailable::Misconfigured(why)) => Err(Error::Input(format!(
                "ReCap is not set up correctly: {why}"
            ))),
        }
    }

    async fn course_ids(&self, course: Option<&str>) -> canvas_core::Result<Vec<u64>> {
        match course {
            Some(text) => Ok(vec![
                canvas_api::resolve_course(&self.client, text).await?.id,
            ]),
            None => Ok(self.client.courses().await?.iter().map(|c| c.id).collect()),
        }
    }

    async fn targets(
        &self,
        target: &str,
        latest: Option<usize>,
    ) -> canvas_core::Result<panopto::RecapTargets> {
        // Capped so one call cannot pull a whole course (tens of GB).
        let latest = latest.unwrap_or(1);
        if !(1..=MAX_LATEST).contains(&latest) {
            return Err(Error::Input(format!(
                "latest must be between 1 and {MAX_LATEST}"
            )));
        }
        panopto::recap_targets(
            &self.client,
            self.recap()?,
            target,
            &self.config.download_root,
            Some(latest),
        )
        .await
    }
}

#[tool_router]
impl CanvasServer {
    /// Builds the server for an already-verified session.
    pub fn new(
        config: Config,
        client: Client,
        recap: Result<Panopto, RecapUnavailable>,
        user: User,
    ) -> Self {
        Self {
            config,
            client,
            recap,
            user,
        }
    }

    /// Reports who is signed in, re-checking the session with Canvas.
    #[tool(
        description = "Check the Canvas and ReCap sessions and return the signed-in user. Call first in a long job."
    )]
    async fn auth_status(&self) -> CallToolResult {
        let recap = match &self.recap {
            Ok(r) => r.check_session().await.is_ok(),
            Err(_) => false,
        };
        match self.client.current_user().await {
            Ok(user) => ok(serde_json::json!({
                "signed_in": true, "user": user, "canvas_url": self.config.canvas_url,
                "recap_signed_in": recap, "download_root": self.config.download_root,
            })),
            Err(e) => fail(e),
        }
    }

    /// Lists active courses.
    #[tool(
        description = "List the user's active Canvas courses. Returns {courses: [...]}, each with id, course_code, name, term."
    )]
    async fn list_courses(&self) -> CallToolResult {
        listed("courses", self.client.courses().await)
    }

    /// Lists assignments due soon.
    #[tool(
        description = "List assignments due within a window (default 7d). Returns {assignments: [...]}, soonest first, each with due_at (RFC 3339 UTC) and submitted."
    )]
    async fn list_assignments(&self, Parameters(args): Parameters<WindowArgs>) -> CallToolResult {
        let within = match window(args.within.as_deref(), "7d") {
            Ok(w) => w,
            Err(e) => return e,
        };
        let course = match args.course.as_deref() {
            Some(text) => match canvas_api::resolve_course(&self.client, text).await {
                Ok(c) => Some(c.id),
                Err(e) => return fail(e),
            },
            None => None,
        };
        listed(
            "assignments",
            canvas_api::upcoming_assignments(&self.client, within, chrono::Utc::now(), course)
                .await,
        )
    }

    /// Lists a course's modules and items.
    #[tool(
        description = "List a course's modules in order. Returns {modules: [...]}, each with items (Page, File, Assignment, ExternalUrl...). Page items carry page_url for read_page."
    )]
    async fn list_modules(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => listed("modules", self.client.modules(c.id).await),
            Err(e) => fail(e),
        }
    }

    /// Lists a course's pages.
    #[tool(
        description = "List a course's wiki pages. Returns {pages: [...]}, each with url slug, title, updated_at. Many courses hide this list; use list_modules then."
    )]
    async fn list_pages(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => listed("pages", self.client.pages(c.id).await),
            Err(e) => fail(e),
        }
    }

    /// Reads one page as Markdown.
    #[tool(description = "Read one Canvas page as Markdown (third-party text).")]
    async fn read_page(&self, Parameters(args): Parameters<PageArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => done(self.client.page(c.id, &args.page).await),
            Err(e) => fail(e),
        }
    }

    /// Lists recent announcements.
    #[tool(
        description = "List announcements posted within a window (default 14d). Returns {announcements: [...]}, newest first, bodies as Markdown."
    )]
    async fn list_announcements(&self, Parameters(args): Parameters<WindowArgs>) -> CallToolResult {
        let since = match window(args.within.as_deref(), "14d") {
            Ok(w) => w,
            Err(e) => return e,
        };
        match self.course_ids(args.course.as_deref()).await {
            Ok(ids) => listed(
                "announcements",
                self.client
                    .announcements(&ids, chrono::Utc::now() - since)
                    .await,
            ),
            Err(e) => fail(e),
        }
    }

    /// Lists a course's discussions.
    #[tool(
        description = "List a course's discussion topics. Returns {discussions: [...]}, each with id, title, opening post, reply and unread counts. Read one in full with read_discussion."
    )]
    async fn list_discussions(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => listed("discussions", self.client.discussions(c.id).await),
            Err(e) => fail(e),
        }
    }

    /// Reads one discussion with every reply.
    #[tool(
        description = "Read a whole discussion: the opening post and every reply, threaded, as Markdown (third-party text). `discussion` is a Canvas discussion link, an id from list_discussions, or part of its title (then pass `course`). Returns the topic (title, body, author, replies count, html_url) with `entries`, oldest first, each with author, posted_at, body, unread and nested `replies`. `note` says why replies are missing."
    )]
    async fn read_discussion(
        &self,
        Parameters(args): Parameters<DiscussionArgs>,
    ) -> CallToolResult {
        match canvas_api::resolve_discussion(&self.client, args.course.as_deref(), &args.discussion)
            .await
        {
            Ok(at) => done(self.client.discussion(at).await),
            Err(e) => fail(e),
        }
    }

    /// Lists calendar events and due dates.
    #[tool(
        description = "List timetabled events and dated assignments within a window (default 7d). Returns {events: [...]}, soonest first."
    )]
    async fn calendar(&self, Parameters(args): Parameters<WindowArgs>) -> CallToolResult {
        let within = match window(args.within.as_deref(), "7d") {
            Ok(w) => w,
            Err(e) => return e,
        };
        let now = chrono::Utc::now();
        match self.course_ids(args.course.as_deref()).await {
            Ok(ids) => listed(
                "events",
                self.client.calendar(&ids, now, now + within).await,
            ),
            Err(e) => fail(e),
        }
    }

    /// Lists a course's quizzes.
    #[tool(
        description = "List a course's classic quizzes (metadata only). Returns {quizzes: [...]}. New Quizzes appear in list_assignments."
    )]
    async fn list_quizzes(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => listed("quizzes", self.client.quizzes(c.id).await),
            Err(e) => fail(e),
        }
    }

    /// Shows grades.
    #[tool(
        description = "Returns {grades: [...]}. Without course: overall grade per course (hidden: true when the course hides totals). With course: per-assignment results."
    )]
    async fn grades(&self, Parameters(args): Parameters<OptionalCourseArgs>) -> CallToolResult {
        match args.course.as_deref() {
            None => listed("grades", self.client.grades().await),
            Some(text) => match canvas_api::resolve_course(&self.client, text).await {
                Ok(c) => listed("grades", self.client.assignment_grades(c.id).await),
                Err(e) => fail(e),
            },
        }
    }

    /// Saves a course's pages as Markdown files.
    #[tool(
        description = "Save a course's pages (or one, by slug) as Markdown files in the download root (<root>/<course>/<module>/<title>.md). Returns `saved` paths; check `failed`."
    )]
    async fn save_pages(&self, Parameters(args): Parameters<SavePagesArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => done(
                canvas_sync::save_pages(
                    &self.client,
                    &c,
                    &self.config.download_root,
                    args.page.as_deref(),
                    |_| {},
                )
                .await,
            ),
            Err(e) => fail(e),
        }
    }

    /// Returns a course's readings grouped by lecture.
    #[tool(
        description = "A course's readings grouped by lecture/week, under `sections`: books, articles, chapters, videos, links and files from Canvas modules and pages, plus Talis Aspire and Leganto reading lists. Each section has module, heading, title, page text and items (kind, title, importance: essential/recommended/optional/further when stated, authors, details, url, doi, isbn, file_id). `lists` names reading lists that could not be read. Third-party text."
    )]
    async fn get_readings(&self, Parameters(args): Parameters<ReadingsArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => done(self.client.readings(c.id, args.lecture.as_deref()).await),
            Err(e) => fail(e),
        }
    }

    /// Lists a course's files.
    #[tool(
        description = "List a course's files (from the Files tab and modules) without downloading. Returns {files: [...], failed}."
    )]
    async fn list_files(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => match canvas_sync::list_files(&self.client, &c).await {
                Ok((files, failed)) => ok(serde_json::json!({ "files": files, "failed": failed })),
                Err(e) => fail(e),
            },
            Err(e) => fail(e),
        }
    }

    /// Downloads a course's new and changed files.
    #[tool(
        description = "Download a course's new and changed files into the download root (<root>/<course>/<module>/). Returns `downloaded` paths; check `failed`."
    )]
    async fn sync_files(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => done(
                canvas_sync::sync_course(&self.client, &c, &self.config.download_root, |_| {})
                    .await,
            ),
            Err(e) => fail(e),
        }
    }

    /// Lists a course's ReCap recordings.
    #[tool(
        description = "List a course's ReCap lecture recordings. Returns {recordings: [...], folders, warning}, newest first, each with id, title, start_time, duration_secs, has_captions."
    )]
    async fn list_recordings(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        let recap = match self.recap() {
            Ok(r) => r,
            Err(e) => return fail(e),
        };
        let course = match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => c,
            Err(e) => return fail(e),
        };
        let Some(code) = course.course_code.as_deref().and_then(panopto::module_code) else {
            return fail(format!(
                "course {} has no module code to find its ReCap folder",
                course.id
            ));
        };
        let term = course.term.as_ref().map(|t| t.name.as_str());
        done(recap.course_recordings(&code, term).await)
    }

    /// Shows one recording's details.
    #[tool(
        description = "Show one ReCap recording: title, length, chapters, captions, and whether an MP4 exists."
    )]
    async fn recording_info(&self, Parameters(args): Parameters<RecordingArgs>) -> CallToolResult {
        let recap = match self.recap() {
            Ok(r) => r,
            Err(e) => return fail(e),
        };
        // Signed media URLs are bearer links; agents get what they need to
        // decide, not the links themselves.
        match recap.recording(&args.recording).await {
            Ok(r) => ok(serde_json::json!({
                "id": r.id, "title": r.title, "folder": r.folder, "start_time": r.start_time,
                "duration_secs": r.duration_secs, "has_mp4": r.mp4_url.is_some(),
                "streams": r.streams.iter().map(|s| s.kind.as_str()).collect::<Vec<_>>(),
                "has_captions": r.has_captions, "caption_languages": r.caption_languages, "chapters": r.chapters, "untrusted_text": true,
            })),
            Err(e) => fail(e),
        }
    }

    /// Returns a recording's transcript text.
    #[tool(
        description = "Return one recording's transcript as readable text with `## chapter` headings (third-party text). Best input for lecture notes."
    )]
    async fn get_transcript(&self, Parameters(args): Parameters<TranscriptArgs>) -> CallToolResult {
        let recap = match self.recap() {
            Ok(r) => r,
            Err(e) => return fail(e),
        };
        match recap.recording(&args.recording).await {
            Ok(rec) => {
                let language = match panopto::Language::parse(
                    args.language.as_deref().unwrap_or("default"),
                ) {
                    Ok(panopto::Language::All) => {
                        return fail(
                            "get_transcript returns one language; use save_transcripts for `all`",
                        );
                    }
                    Ok(l) => l,
                    Err(e) => return fail(e),
                };
                let code = match panopto::languages(&rec, language) {
                    Ok(codes) => codes[0],
                    Err(e) => return fail(e),
                };
                match recap.transcript_in(&rec, code).await {
                    Ok(t) => ok(serde_json::json!({
                        "id": t.id, "title": rec.title, "language": t.language, "text": t.text, "untrusted_text": true
                    })),
                    Err(e) => fail(e),
                }
            }
            Err(e) => fail(e),
        }
    }

    /// Saves transcripts to disk.
    #[tool(
        description = "Save .srt and .txt transcripts for a recording or a course's newest `latest` recordings (default 1) into the download root."
    )]
    async fn save_transcripts(
        &self,
        Parameters(args): Parameters<RecapTargetArgs>,
    ) -> CallToolResult {
        let targets = match self.targets(&args.target, args.latest).await {
            Ok(t) => t,
            Err(e) => return fail(e),
        };
        let Ok(recap) = self.recap() else {
            return fail(Error::SessionExpired);
        };
        let language = match panopto::Language::parse(args.language.as_deref().unwrap_or("default"))
        {
            Ok(l) => l,
            Err(e) => return fail(e),
        };
        let mut saved = Vec::new();
        let mut no_captions = Vec::new();
        let mut failed = Vec::new();
        for id in &targets.ids {
            let rec = match recap.recording(id).await {
                Ok(r) => r,
                Err(e @ Error::SessionExpired) => return fail(e),
                Err(e) => {
                    failed.push(serde_json::json!({ "id": id, "error": e.to_string() }));
                    continue;
                }
            };
            match recap.save_transcripts(&rec, &targets.dir, language).await {
                Ok(report) => {
                    saved.extend(report.written);
                    for (code, e) in report.failed {
                        failed.push(serde_json::json!({ "id": id, "language": code, "error": e }));
                    }
                }
                Err(e @ Error::SessionExpired) => return fail(e),
                Err(Error::NotFound(_)) => no_captions.push(id.clone()),
                Err(e) => failed.push(serde_json::json!({ "id": id, "error": e.to_string() })),
            }
        }
        ok(serde_json::json!({
            "saved": saved, "no_captions": no_captions, "failed": failed, "warning": targets.warning
        }))
    }

    /// Downloads recordings.
    #[tool(
        description = "Download a recording, or a course's newest `latest` recordings (default 1), as MP4 into the download root. Large (about 150 MB per hour); prefer get_transcript for notes."
    )]
    async fn download_recordings(
        &self,
        Parameters(args): Parameters<DownloadArgs>,
    ) -> CallToolResult {
        let targets = match self.targets(&args.target, args.latest).await {
            Ok(t) => t,
            Err(e) => return fail(e),
        };
        let Ok(recap) = self.recap() else {
            return fail(Error::SessionExpired);
        };
        let options = panopto::DownloadOptions {
            streams: args.streams,
            no_mp4: false,
        };
        let mut downloaded = Vec::new();
        let mut failed = Vec::new();
        for id in &targets.ids {
            let result = async {
                let rec = recap.recording(id).await?;
                recap.download(&rec, &targets.dir, &options, |_| {}).await
            }
            .await;
            match result {
                Ok(d) => downloaded.push(d),
                Err(e @ Error::SessionExpired) => return fail(e),
                Err(e) => failed.push(serde_json::json!({ "id": id, "error": e.to_string() })),
            }
        }
        ok(
            serde_json::json!({ "downloaded": downloaded, "failed": failed, "warning": targets.warning }),
        )
    }

    /// What is new and due.
    #[tool(
        description = "What changed since the last digest (announcements, new or changed assignments, files, recordings) plus unsubmitted work due soon. One call for notifications."
    )]
    async fn digest(&self, Parameters(args): Parameters<DigestArgs>) -> CallToolResult {
        let upcoming = match window(args.upcoming.as_deref(), "7d") {
            Ok(w) => w,
            Err(e) => return e,
        };
        let root = &self.config.download_root;
        let now = chrono::Utc::now();
        let (since, from_marker) = match canvas_digest::resolve_since(
            args.since.as_deref().unwrap_or("last"),
            root,
            now,
            chrono::Duration::days(7),
        ) {
            Ok(v) => v,
            Err(e) => return fail(e),
        };
        let options = canvas_digest::Options {
            since,
            upcoming,
            files: args.files.unwrap_or(true),
            recordings: args.recordings.unwrap_or(true),
        };
        let (previous, note) = canvas_digest::load_state(root, now);
        let previous = previous.filter(|_| from_marker);
        match canvas_digest::digest(
            &self.client,
            self.recap.as_ref().ok(),
            &options,
            previous.as_ref(),
        )
        .await
        {
            Ok((mut d, state)) => {
                d.skipped.extend(note);
                if from_marker && let Err(e) = canvas_digest::save_state(root, &state) {
                    d.skipped.push(format!("digest state not saved: {e}"));
                }
                ok(d)
            }
            Err(e) => fail(e),
        }
    }

    /// Returns how to update canvas-cli to the newest version.
    #[tool(
        description = "Check whether a newer canvas-cli exists (installed vs GitHub, with the list of changes) and return the steps to update it. Call when the user asks to update, or a tool seems missing."
    )]
    async fn update(&self) -> CallToolResult {
        let status = canvas_api::check_for_update().await;
        ok(serde_json::json!({
            "status": status,
            "one_command": "canvas update --apply",
            "prompt": UPDATE_PROMPT,
            "latest_steps_url": "https://github.com/Al3xWalton/canvas-cli/blob/main/UPDATE.md",
            "steps": UPDATE_STEPS,
        }))
    }

    /// Returns the agent guide.
    #[tool(
        description = "Return the guide to these tools: what each does, recipes and error handling."
    )]
    async fn usage(&self) -> CallToolResult {
        CallToolResult::success(vec![ContentBlock::text(GUIDE)])
    }
}

#[tool_handler]
impl rmcp::ServerHandler for CanvasServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new("canvas-mcp", env!("CARGO_PKG_VERSION")))
            .with_instructions(format!("{GUIDE}\nSigned in as {}.", self.user.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    /// One tool that returns a list.
    struct ListTool {
        name: &'static str,
        arguments: Value,
        /// The key its list comes back under.
        key: &'static str,
        /// A field of the first item and the value the fixture gives it,
        /// proving the right fixture reached the right tool.
        first: (&'static str, Value),
    }

    /// Every tool that returns a list.
    fn list_tools() -> Vec<ListTool> {
        let course = || json!({ "course": "ABC1020" });
        let tool = |name, arguments, key, field, value| ListTool {
            name,
            arguments,
            key,
            first: (field, value),
        };
        vec![
            tool(
                "list_courses",
                json!({}),
                "courses",
                "course_code",
                json!("ABC1020"),
            ),
            tool(
                "list_assignments",
                json!({}),
                "assignments",
                "name",
                json!("Essay"),
            ),
            tool("list_modules", course(), "modules", "name", json!("Week 1")),
            tool("list_pages", course(), "pages", "url", json!("intro")),
            tool(
                "list_announcements",
                json!({}),
                "announcements",
                "title",
                json!("Welcome"),
            ),
            tool(
                "list_discussions",
                course(),
                "discussions",
                "title",
                json!("Questions"),
            ),
            tool("calendar", json!({}), "events", "title", json!("Lecture 1")),
            tool(
                "list_quizzes",
                course(),
                "quizzes",
                "title",
                json!("Quiz 1"),
            ),
            tool("grades", json!({}), "grades", "current_score", json!(71.5)),
            tool("grades", course(), "grades", "score", json!(8.0)),
        ]
    }

    #[test]
    fn ok_always_gives_an_object() {
        let structured = |r: CallToolResult| r.structured_content.unwrap();
        assert_eq!(structured(ok(vec![1, 2])), json!({ "result": [1, 2] }));
        assert_eq!(structured(ok(5)), json!({ "result": 5 }));
        assert_eq!(structured(ok("text")), json!({ "result": "text" }));
        assert_eq!(structured(ok(None::<u8>)), json!({ "result": null }));
        assert_eq!(structured(ok(json!({ "a": 1 }))), json!({ "a": 1 }));
        let courses = listed("courses", Ok(vec![json!({ "id": 1 })]));
        assert_eq!(structured(courses), json!({ "courses": [{ "id": 1 }] }));
        let five = ok(5);
        assert_eq!(five.content[0].as_text().unwrap().text, r#"{"result":5}"#);
    }

    #[test]
    fn guide_names_each_list_key() {
        for ListTool { name, key, .. } in list_tools() {
            // Search after the tool's own name, which may equal its key.
            let start = GUIDE.find(&format!("- `{name}`")).unwrap();
            let rest = &GUIDE[start + name.len() + 4..];
            let end = ["\n- ", "\n\n"]
                .iter()
                .filter_map(|s| rest.find(s))
                .min()
                .unwrap_or(rest.len());
            assert!(
                rest[..end].contains(&format!("`{key}`")),
                "guide entry for {name} does not name `{key}`"
            );
        }
    }

    /// A tiny Canvas stand-in on localhost: one course (`ABC1020`) with one
    /// of everything the list tools read. No real network.
    async fn fake_canvas() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let due = (chrono::Utc::now() + chrono::Duration::days(2)).to_rfc3339();
        tokio::spawn(async move {
            loop {
                let (mut socket, _) = listener.accept().await.unwrap();
                let due = due.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buf = [0; 4096];
                    while !request.windows(4).any(|w| w == b"\r\n\r\n") {
                        let n = socket.read(&mut buf).await.unwrap();
                        if n == 0 {
                            break;
                        }
                        request.extend_from_slice(&buf[..n]);
                    }
                    let request = String::from_utf8_lossy(&request).into_owned();
                    let target = request.split_whitespace().nth(1).unwrap_or("");
                    let body = canvas_fixture(target, &due);
                    let status = if body.is_empty() {
                        "404 Not Found"
                    } else {
                        "200 OK"
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = socket.write_all(response.as_bytes()).await;
                });
            }
        });
        base
    }

    /// The fixture body for one Canvas API request target. Paths match
    /// exactly, so an unexpected request gets an empty body (a 404) and
    /// fails its tool rather than decoding someone else's fixture.
    fn canvas_fixture(target: &str, due: &str) -> String {
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        match path {
            "/api/v1/users/self" => r#"{"id":7,"name":"Test Student"}"#.into(),
            "/api/v1/courses" if query.contains("include[]=total_scores") => {
                r#"[{"id":1,"name":"Example Studies","course_code":"ABC1020","enrollments":[{"type":"student","computed_current_score":71.5,"computed_current_grade":"B"}]}]"#.into()
            }
            "/api/v1/courses" => r#"[{"id":1,"name":"Example Studies","course_code":"ABC1020","term":{"id":3,"name":"2026/27"}}]"#.into(),
            "/api/v1/courses/1/assignments" => format!(
                r#"[{{"id":11,"course_id":1,"name":"Essay","due_at":"{due}","html_url":"https://canvas.example/courses/1/assignments/11"}}]"#
            ),
            "/api/v1/courses/1/modules" => r#"[{"id":21,"name":"Week 1","position":1,"items":[{"id":22,"title":"Intro","type":"Page","page_url":"intro"}]}]"#.into(),
            "/api/v1/courses/1/pages" => r#"[{"url":"intro","title":"Intro"}]"#.into(),
            "/api/v1/announcements" => r#"[{"id":31,"title":"Welcome","message":"<p>Hello</p>","context_code":"course_1"}]"#.into(),
            "/api/v1/courses/1/discussion_topics" => r#"[{"id":41,"title":"Questions"}]"#.into(),
            "/api/v1/courses/1/discussion_topics/41" => r#"{"id":41,"title":"Questions","message":"<p>Ask here</p>","discussion_subentry_count":2,"user_can_see_posts":true,"group_topic_children":[]}"#.into(),
            "/api/v1/courses/1/discussion_topics/41/view" if query == "include_new_entries=1" => r#"{"participants":[{"id":7,"display_name":"Test Student"}],"unread_entries":[43],"view":[{"id":42,"user_id":7,"created_at":"2026-10-01T09:00:00Z","message":"<p>First</p>","replies":[{"id":43,"user_id":7,"parent_id":42,"message":"<p>Second</p>"}]}],"new_entries":[]}"#.into(),
            "/api/v1/calendar_events" => r#"[{"id":51,"type":"event","title":"Lecture 1","location_name":"Room 1"}]"#.into(),
            "/api/v1/courses/1/quizzes" => r#"[{"id":61,"title":"Quiz 1"}]"#.into(),
            "/api/v1/courses/1/students/submissions" => r#"[{"assignment_id":11,"score":8.0,"assignment":{"name":"Essay","points_possible":10.0}}]"#.into(),
            _ => String::new(),
        }
    }

    /// Sends one JSON-RPC request and returns its `result`, skipping any
    /// notifications or requests the server sends first. Gives up after
    /// 10 seconds rather than hang.
    async fn rpc(
        to_server: &mut (impl tokio::io::AsyncWrite + Unpin),
        from_server: &mut tokio::io::Lines<impl tokio::io::AsyncBufRead + Unpin>,
        id: u64,
        method: &str,
        params: Value,
    ) -> Value {
        let request = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        to_server
            .write_all(format!("{request}\n").as_bytes())
            .await
            .unwrap();
        loop {
            let wait = std::time::Duration::from_secs(10);
            let line = tokio::time::timeout(wait, from_server.next_line())
                .await
                .expect("the server answers within 10 seconds")
                .unwrap()
                .unwrap();
            let message: Value = serde_json::from_str(&line).unwrap();
            // Requests from the server number their own ids; skip them.
            if message["id"] == json!(id) && message.get("method").is_none() {
                assert!(message["error"].is_null(), "{method} failed: {message}");
                return message["result"].clone();
            }
        }
    }

    /// Calls every list tool over MCP's JSON-RPC wire format, as a client
    /// does, and checks that `structuredContent` is an object holding the
    /// fixture's list under the documented key.
    #[tokio::test]
    async fn list_tools_return_objects_over_the_wire() {
        let base = fake_canvas().await;
        let config = Config {
            canvas_url: base.clone(),
            panopto_host: "example.hosted.panopto.com".into(),
            download_root: std::env::temp_dir().join("canvas-mcp-wire-test"),
        };
        let session = canvas_auth::Session {
            captured_at: 0,
            cookies: vec![canvas_auth::Cookie {
                name: "canvas_session".into(),
                value: "x".into(),
                domain: "127.0.0.1".into(),
                path: "/".into(),
                expires: None,
            }],
        };
        let client = Client::new(&config, &session).unwrap();
        let user = client.current_user().await.unwrap();
        let server = CanvasServer::new(config, client, Err(RecapUnavailable::SignedOut), user);

        let (client_end, server_end) = tokio::io::duplex(1 << 20);
        tokio::spawn(async move {
            use rmcp::ServiceExt;
            server
                .serve(server_end)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let (read, mut write) = tokio::io::split(client_end);
        let mut lines = BufReader::new(read).lines();

        let init = json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "wire-test", "version": "0" },
        });
        rpc(&mut write, &mut lines, 0, "initialize", init).await;
        let initialized = json!({ "jsonrpc": "2.0", "method": "notifications/initialized" });
        write
            .write_all(format!("{initialized}\n").as_bytes())
            .await
            .unwrap();

        let listing = rpc(&mut write, &mut lines, 1, "tools/list", json!({})).await;
        for (id, tool) in (2..).zip(list_tools()) {
            let (name, key) = (tool.name, tool.key);
            let description = listing["tools"]
                .as_array()
                .unwrap()
                .iter()
                .find(|t| t["name"] == name)
                .and_then(|t| t["description"].as_str())
                .unwrap();
            assert!(
                description.contains(&format!("{{{key}: [...]")),
                "{name} description does not name `{key}`: {description}"
            );

            let params = json!({ "name": name, "arguments": tool.arguments });
            let result = rpc(&mut write, &mut lines, id, "tools/call", params).await;
            assert_ne!(result["isError"], json!(true), "{name}: {result}");
            let structured = &result["structuredContent"];
            assert!(structured.is_object(), "{name} gave {structured}");
            let (field, value) = tool.first;
            assert_eq!(
                structured[key][0][field], value,
                "{name} `{key}` list is not the fixture's: {structured}"
            );
        }

        // read_discussion, named by link and by title, returns the thread.
        let link = format!("{base}/courses/1/discussion_topics/41?module_item_id=9");
        for (id, arguments) in [
            (100, json!({ "discussion": link })),
            (
                101,
                json!({ "discussion": "questions", "course": "ABC1020" }),
            ),
        ] {
            let params = json!({ "name": "read_discussion", "arguments": arguments });
            let result = rpc(&mut write, &mut lines, id, "tools/call", params).await;
            assert_ne!(result["isError"], json!(true), "{result}");
            let thread = &result["structuredContent"];
            assert_eq!(thread["title"], "Questions", "{thread}");
            assert_eq!(thread["body"], "Ask here");
            assert_eq!(thread["untrusted_text"], true);
            let first = &thread["entries"][0];
            assert_eq!(first["author"], "Test Student");
            assert_eq!(first["replies"][0]["body"], "Second");
            assert_eq!(first["replies"][0]["unread"], true);
        }
        let params = json!({ "name": "read_discussion", "arguments": { "discussion": "41" } });
        let result = rpc(&mut write, &mut lines, 102, "tools/call", params).await;
        assert_eq!(result["isError"], json!(true), "an id needs a course");
    }

    #[test]
    fn update_prompt_matches_the_readme() {
        let readme = include_str!("../../../README.md");
        assert!(
            readme.contains(UPDATE_PROMPT),
            "README update prompt differs"
        );
        assert!(UPDATE_STEPS.contains("make install"));
    }

    #[test]
    fn expired_session_reaches_the_agent_with_the_fix() {
        let result = fail(canvas_core::Error::SessionExpired);
        assert_eq!(result.is_error, Some(true));
        let text = format!("{:?}", result.content);
        assert!(text.contains("canvas auth login"));
    }

    #[test]
    fn guide_names_every_tool() {
        for tool in [
            "auth_status",
            "list_courses",
            "list_assignments",
            "list_modules",
            "list_pages",
            "read_page",
            "save_pages",
            "get_readings",
            "list_announcements",
            "list_discussions",
            "read_discussion",
            "calendar",
            "list_quizzes",
            "grades",
            "list_files",
            "sync_files",
            "list_recordings",
            "recording_info",
            "get_transcript",
            "save_transcripts",
            "download_recordings",
            "update",
        ] {
            assert!(GUIDE.contains(tool), "{tool} missing from guide");
        }
    }

    /// No tool argument may name a location on disk: files always land in
    /// the configured download root.
    #[test]
    fn no_tool_argument_is_a_path() {
        let schemas = [
            schemars::schema_for!(CourseArgs),
            schemars::schema_for!(WindowArgs),
            schemars::schema_for!(OptionalCourseArgs),
            schemars::schema_for!(PageArgs),
            schemars::schema_for!(DiscussionArgs),
            schemars::schema_for!(SavePagesArgs),
            schemars::schema_for!(ReadingsArgs),
            schemars::schema_for!(RecordingArgs),
            schemars::schema_for!(TranscriptArgs),
            schemars::schema_for!(RecapTargetArgs),
            schemars::schema_for!(DownloadArgs),
            schemars::schema_for!(DigestArgs),
        ];
        for schema in schemas {
            let props = schema.as_value()["properties"]
                .as_object()
                .cloned()
                .unwrap_or_default();
            for name in props.keys() {
                let lower = name.to_lowercase();
                let pathy = [
                    "path",
                    "dir",
                    "directory",
                    "root",
                    "dest",
                    "destination",
                    "output",
                    "folder",
                ]
                .contains(&lower.as_str())
                    || ["_path", "_dir", "_root", "_dest", "_file"]
                        .iter()
                        .any(|suffix| lower.ends_with(suffix));
                assert!(!pathy, "argument `{name}` looks like a path");
            }
        }
    }

    #[test]
    fn traversal_in_ids_and_slugs_is_refused() {
        // Recording targets must be UUIDs; anything else is a course name,
        // which is matched against Canvas, never used as a path.
        assert!(panopto::delivery_id("../../etc/passwd").is_err());
        assert!(panopto::delivery_id("0a1b2c3d-1111-4222-8333-444455556666/../x").is_err());
    }
}
