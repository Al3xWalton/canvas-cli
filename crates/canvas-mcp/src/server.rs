//! The MCP tool definitions.
//!
//! Each tool is a thin wrapper over the same library call the matching CLI
//! command makes. Results are structured JSON. Failures become MCP tool
//! errors (not protocol errors) so the agent reads the message, including
//! the `canvas auth login` instruction when the session has expired.
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

/// A course's pages, or one of them.
#[derive(Debug, Default, Deserialize, schemars::JsonSchema)]
pub struct SavePagesArgs {
    /// Course id, code or part of the name.
    pub course: String,
    /// Only this page slug (default: every page).
    #[serde(default)]
    pub page: Option<String>,
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
fn ok(value: impl serde::Serialize) -> CallToolResult {
    match serde_json::to_value(value) {
        Ok(v) => CallToolResult::structured(v),
        Err(e) => fail(e),
    }
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
    #[tool(description = "List the user's active Canvas courses (id, course_code, name, term).")]
    async fn list_courses(&self) -> CallToolResult {
        done(self.client.courses().await)
    }

    /// Lists assignments due soon.
    #[tool(
        description = "List assignments due within a window (default 7d), soonest first, with due_at (RFC 3339 UTC) and submitted."
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
        done(
            canvas_api::upcoming_assignments(&self.client, within, chrono::Utc::now(), course)
                .await,
        )
    }

    /// Lists a course's modules and items.
    #[tool(
        description = "List a course's modules in order, with items (Page, File, Assignment, ExternalUrl...). Page items carry page_url for read_page."
    )]
    async fn list_modules(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => done(self.client.modules(c.id).await),
            Err(e) => fail(e),
        }
    }

    /// Lists a course's pages.
    #[tool(
        description = "List a course's wiki pages (url slug, title, updated_at). Many courses hide this list; use list_modules then."
    )]
    async fn list_pages(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => done(self.client.pages(c.id).await),
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
        description = "List announcements posted within a window (default 14d), newest first, bodies as Markdown."
    )]
    async fn list_announcements(&self, Parameters(args): Parameters<WindowArgs>) -> CallToolResult {
        let since = match window(args.within.as_deref(), "14d") {
            Ok(w) => w,
            Err(e) => return e,
        };
        match self.course_ids(args.course.as_deref()).await {
            Ok(ids) => done(
                self.client
                    .announcements(&ids, chrono::Utc::now() - since)
                    .await,
            ),
            Err(e) => fail(e),
        }
    }

    /// Lists a course's discussions.
    #[tool(description = "List a course's discussion topics with reply and unread counts.")]
    async fn list_discussions(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => done(self.client.discussions(c.id).await),
            Err(e) => fail(e),
        }
    }

    /// Lists calendar events and due dates.
    #[tool(
        description = "List timetabled events and dated assignments within a window (default 7d), soonest first."
    )]
    async fn calendar(&self, Parameters(args): Parameters<WindowArgs>) -> CallToolResult {
        let within = match window(args.within.as_deref(), "7d") {
            Ok(w) => w,
            Err(e) => return e,
        };
        let now = chrono::Utc::now();
        match self.course_ids(args.course.as_deref()).await {
            Ok(ids) => done(self.client.calendar(&ids, now, now + within).await),
            Err(e) => fail(e),
        }
    }

    /// Lists a course's quizzes.
    #[tool(
        description = "List a course's classic quizzes (metadata only). New Quizzes appear in list_assignments."
    )]
    async fn list_quizzes(&self, Parameters(args): Parameters<CourseArgs>) -> CallToolResult {
        match canvas_api::resolve_course(&self.client, &args.course).await {
            Ok(c) => done(self.client.quizzes(c.id).await),
            Err(e) => fail(e),
        }
    }

    /// Shows grades.
    #[tool(
        description = "Without course: overall grade per course (hidden: true when the course hides totals). With course: per-assignment results."
    )]
    async fn grades(&self, Parameters(args): Parameters<OptionalCourseArgs>) -> CallToolResult {
        match args.course.as_deref() {
            None => done(self.client.grades().await),
            Some(text) => match canvas_api::resolve_course(&self.client, text).await {
                Ok(c) => done(self.client.assignment_grades(c.id).await),
                Err(e) => fail(e),
            },
        }
    }

    /// Saves a course's pages as Markdown files.
    #[tool(
        description = "Save a course's pages (or one, by slug) as Markdown files in the download root (<root>/<course>/<module>/<title>.md). Returns paths; check `failed`."
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

    /// Lists a course's files.
    #[tool(
        description = "List a course's files (from the Files tab and modules) without downloading."
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
        description = "Download a course's new and changed files into the download root (<root>/<course>/<module>/). Returns paths; check `failed`."
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
        description = "List a course's ReCap lecture recordings, newest first (id, title, start_time, duration_secs, has_captions)."
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
            "list_announcements",
            "list_discussions",
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
            schemars::schema_for!(SavePagesArgs),
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
