//! `canvas`: command-line access to Canvas and Panopto ReCap.
//!
//! This binary is a thin layer: it parses arguments, loads the config,
//! calls the library crates and prints text or `--json`. All logic lives
//! in the libraries so the MCP server behaves the same way. Errors are
//! printed as one plain line with no backtrace, and the exit code is
//! non-zero.

use std::io::Write;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

use canvas_core::Config;
use clap::{Parser, Subcommand};
use serde::Serialize;

/// Writes a line to stdout. If stdout is closed (e.g. piped into `head`),
/// exits quietly instead of panicking.
/// Canvas text is third-party, so control characters (terminal escape
/// codes) are removed; newlines and tabs are kept.
macro_rules! out {
    ($($arg:tt)*) => {
        if writeln!(std::io::stdout(), "{}", clean(&format!($($arg)*))).is_err() {
            std::process::exit(0);
        }
    };
}

/// Removes control characters other than newline and tab.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect()
}

/// Read Canvas, download course files and ReCap lectures.
#[derive(Debug, Parser)]
#[command(name = "canvas", version, about)]
struct Cli {
    /// Print machine-readable JSON instead of text.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Command,
}

/// Top-level commands.
#[derive(Debug, Subcommand)]
enum Command {
    /// Sign in, check or remove the stored browser session.
    #[command(subcommand)]
    Auth(AuthCommand),
    /// Show the active config file and settings.
    Config,
    /// Connect canvas-mcp to an agent client.
    #[command(subcommand)]
    Mcp(McpCommand),
    /// List your active courses.
    Courses,
    /// List assignments due soon across your courses.
    Assignments {
        /// How far ahead to look: a number with m, h, d or w.
        #[arg(long, default_value = "7d", value_parser = canvas_api::parse_window)]
        due: chrono::Duration,
        /// Only this course: an id, code or part of the name.
        #[arg(long)]
        course: Option<String>,
    },
    /// List a course's modules and their items.
    Modules {
        /// Course id, code (e.g. ABC1020) or part of the name.
        course: String,
    },
    /// List a course's pages, or show one as Markdown.
    Pages {
        /// Course id, code or part of the name.
        course: String,
        /// Page url slug to show (from the list).
        page: Option<String>,
    },
    /// List recent announcements.
    Announcements {
        /// How far back to look: a number with m, h, d or w.
        #[arg(long, default_value = "14d", value_parser = canvas_api::parse_window)]
        since: chrono::Duration,
        /// Only this course: an id, code or part of the name.
        #[arg(long)]
        course: Option<String>,
    },
    /// List timetabled events and due dates (undated work is not shown).
    Calendar {
        /// How far ahead to look: a number with m, h, d or w.
        #[arg(long, default_value = "7d", value_parser = canvas_api::parse_window)]
        days: chrono::Duration,
        /// Only this course: an id, code or part of the name.
        #[arg(long)]
        course: Option<String>,
    },
    /// List a course's quizzes (metadata only).
    Quizzes {
        /// Course id, code or part of the name.
        course: String,
    },
    /// Show overall grades, or one course's assignment results.
    Grades {
        /// Course id, code or part of the name.
        course: Option<String>,
    },
    /// What is new since the last run, plus what is due soon.
    Digest {
        /// `last` (since the previous `--since last` run) or a window like 2d.
        #[arg(long, default_value = "last")]
        since: String,
        /// Also list unsubmitted work due within this window.
        #[arg(long, default_value = "7d", value_parser = canvas_api::parse_window)]
        upcoming: chrono::Duration,
        /// Skip course files (faster).
        #[arg(long)]
        no_files: bool,
        /// Skip ReCap recordings.
        #[arg(long)]
        no_recordings: bool,
    },
    /// Panopto ReCap lecture recordings.
    #[command(subcommand)]
    Recap(RecapCommand),
    /// List or download course files.
    #[command(subcommand)]
    Files(FilesCommand),
    /// List a course's discussion topics.
    Discussions {
        /// Course id, code or part of the name.
        course: String,
    },
}

/// `canvas auth ...`
#[derive(Debug, Subcommand)]
enum AuthCommand {
    /// Open Chrome, sign in to Canvas and ReCap, and store the session.
    Login {
        /// Skip the ReCap (Panopto) sign-in.
        #[arg(long, conflicts_with = "recap_only")]
        no_recap: bool,
        /// Renew only the ReCap sign-in, keeping the Canvas session.
        #[arg(long)]
        recap_only: bool,
        /// Seconds to wait for each sign-in step.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Check that the stored session still works.
    Status,
    /// Delete the stored session from the keychain.
    Logout,
}

/// `canvas recap ...`
#[derive(Debug, Subcommand)]
enum RecapCommand {
    /// List a course's recordings, newest first.
    List {
        /// Course id, code (e.g. ABC1016) or part of the name.
        course: String,
    },
    /// Download recordings as MP4: one recording, or a whole course.
    Download {
        /// Viewer URL or delivery ID, or a course id, code or name.
        target: String,
        /// Also save the separate camera and screen streams (needs ffmpeg).
        #[arg(long)]
        streams: bool,
        /// Skip the composite MP4 (use with --streams).
        #[arg(long, requires = "streams")]
        no_mp4: bool,
        /// For a course, only the newest N recordings.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        latest: Option<u64>,
        /// Download here instead of the configured download_root.
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// Save captions as .srt and readable .txt (with chapter headings).
    Transcript {
        /// Viewer URL or delivery ID, or a course id, code or name.
        target: String,
        /// For a course, only the newest N recordings.
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..))]
        latest: Option<u64>,
        /// Print one recording's transcript text instead of saving files.
        #[arg(long, conflicts_with_all = ["latest", "root"])]
        stdout: bool,
        /// Caption language: a code from `recap info`, or `all`. Default:
        /// the recording's first language.
        #[arg(long, default_value = "default", value_parser = |s: &str| panopto::Language::parse(s).map_err(|e| e.to_string()))]
        language: panopto::Language,
        /// Save here instead of the configured download_root.
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
    /// Show a recording's details and streams.
    Info {
        /// Viewer or embed URL, or the delivery ID.
        recording: String,
    },
}

/// `canvas files ...`
#[derive(Debug, Subcommand)]
enum FilesCommand {
    /// List a course's files (from the Files tab and from modules).
    List {
        /// Course id, code or part of the name.
        course: String,
    },
    /// Download new and changed files to the download root.
    Sync {
        /// Course id, code or part of the name. Omit with --all.
        course: Option<String>,
        /// Sync every active course.
        #[arg(long, conflicts_with = "course")]
        all: bool,
        /// Download here instead of the configured download_root.
        #[arg(long)]
        root: Option<std::path::PathBuf>,
    },
}

/// `canvas mcp ...`
#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Print the command or config that adds canvas-mcp to a client.
    Install {
        /// Which client to set up.
        #[arg(long, value_enum)]
        client: McpClient,
    },
}

/// MCP clients `canvas mcp install` knows about.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum McpClient {
    /// Claude Code.
    Claude,
    /// OpenAI Codex CLI.
    Codex,
    /// Cursor, or any client that reads `mcpServers` JSON.
    Cursor,
}

/// What `canvas auth status` reports.
#[derive(Debug, Serialize)]
struct Status {
    signed_in: bool,
    user: Option<canvas_api::User>,
    canvas_url: String,
    recap_session: bool,
    captured_at: u64,
    canvas_expires_at: Option<u64>,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let path = Config::default_path()?;
    let config = Config::load(&path)?;
    match cli.command {
        Command::Config => {
            if cli.json {
                print_json(&serde_json::json!({ "path": path, "config": config }));
            } else {
                out!("config         {}", path.display());
                out!("canvas_url     {}", config.canvas_url);
                out!("panopto_host   {}", config.panopto_host);
                out!("download_root  {}", config.download_root.display());
            }
        }
        Command::Auth(AuthCommand::Login {
            no_recap,
            recap_only,
            timeout,
        }) => {
            if recap_only {
                // Fail early (before opening Chrome) if there is no session.
                canvas_auth::load().map_err(|_| {
                    anyhow::anyhow!("no stored session; run `canvas auth login` first")
                })?;
                let options = canvas_auth::LoginOptions {
                    timeout: std::time::Duration::from_secs(timeout),
                    canvas: false,
                    ..Default::default()
                };
                let fresh = canvas_auth::login(&config, &options, |m| eprintln!("{m}")).await?;
                // Re-read now: the stored session may have changed while the
                // browser was open; only the ReCap cookies are replaced.
                let mut session = canvas_auth::load()?;
                session.replace_host(&config.panopto_host, &fresh);
                canvas_auth::store(&session)?;
                if cli.json {
                    print_json(&serde_json::json!({ "recap_signed_in": true }));
                } else {
                    out!("ReCap signed in again. Canvas session kept.");
                }
                return Ok(());
            }
            let options = canvas_auth::LoginOptions {
                timeout: std::time::Duration::from_secs(timeout),
                panopto: !no_recap,
                ..Default::default()
            };
            let session = canvas_auth::login(&config, &options, |m| eprintln!("{m}")).await?;
            canvas_auth::store(&session)?;
            let user = canvas_api::Client::new(&config, &session)?
                .current_user()
                .await?;
            if cli.json {
                print_json(&serde_json::json!({ "signed_in": true, "user": user }));
            } else {
                out!(
                    "Signed in as {}. Session stored in the keychain.",
                    user.name
                );
            }
        }
        Command::Auth(AuthCommand::Status) => {
            let session = canvas_auth::load()?;
            let user = canvas_api::Client::new(&config, &session)?
                .current_user()
                .await?;
            let canvas_host = url::Url::parse(&config.canvas_url)
                .ok()
                .and_then(|u| u.host_str().map(str::to_owned))
                .unwrap_or_default();
            let status = Status {
                signed_in: true,
                user: Some(user),
                canvas_url: config.canvas_url.clone(),
                recap_session: session.covers(&config.panopto_host),
                captured_at: session.captured_at,
                canvas_expires_at: session.earliest_expiry(&canvas_host).map(|t| t as u64),
            };
            if cli.json {
                print_json(&status);
            } else {
                let name = status.user.as_ref().map_or("?", |u| u.name.as_str());
                out!("Signed in to {} as {name}", status.canvas_url);
                out!(
                    "ReCap session  {}",
                    if status.recap_session { "yes" } else { "no" }
                );
                out!("Captured       {}", ago(status.captured_at));
                match status.canvas_expires_at {
                    Some(t) => out!("Expires        in {}", until(t)),
                    None => out!("Expires        when Canvas ends the session"),
                }
            }
        }
        Command::Modules { course } => {
            let client = client(&config)?;
            let course = canvas_api::resolve_course(&client, &course).await?;
            let modules = client.modules(course.id).await?;
            if cli.json {
                print_json(&modules);
            } else {
                for m in &modules {
                    out!("{}", m.name);
                    for item in m.items.iter().flatten() {
                        out!("  {:<13} {}", item.kind, item.title);
                    }
                }
            }
        }
        Command::Pages { course, page } => {
            let client = client(&config)?;
            let course = canvas_api::resolve_course(&client, &course).await?;
            match page {
                Some(slug) => {
                    let page = client.page(course.id, &slug).await?;
                    if cli.json {
                        print_json(&page);
                    } else {
                        out!("# {}\n\n{}", page.summary.title, page.body);
                    }
                }
                None => {
                    let pages = client.pages(course.id).await?;
                    if cli.json {
                        print_json(&pages);
                    } else {
                        for p in &pages {
                            out!("{:<40} {}", p.url, p.title);
                        }
                    }
                }
            }
        }
        Command::Announcements { since, course } => {
            let client = client(&config)?;
            let ids: Vec<u64> = match course {
                Some(text) => vec![canvas_api::resolve_course(&client, &text).await?.id],
                None => client.courses().await?.iter().map(|c| c.id).collect(),
            };
            let list = client
                .announcements(&ids, chrono::Utc::now() - since)
                .await?;
            if cli.json {
                print_json(&list);
            } else if list.is_empty() {
                out!("No announcements.");
            } else {
                for a in &list {
                    let when = a.posted_at.map(local_time).unwrap_or_default();
                    out!("{when}  {}\n{}\n", a.title, a.body);
                }
            }
        }
        Command::Calendar { days, course } => {
            let client = client(&config)?;
            let ids: Vec<u64> = match course {
                Some(text) => vec![canvas_api::resolve_course(&client, &text).await?.id],
                None => client.courses().await?.iter().map(|c| c.id).collect(),
            };
            let now = chrono::Utc::now();
            let events = client.calendar(&ids, now, now + days).await?;
            if cli.json {
                print_json(&events);
            } else if events.is_empty() {
                out!("Nothing on the calendar.");
            } else {
                for e in &events {
                    let when = e.start_at.map(local_time).unwrap_or_default();
                    let place = e.location_name.as_deref().unwrap_or("");
                    out!("{when:<17} {:<10} {} {place}", e.kind, e.title);
                }
            }
        }
        Command::Quizzes { course } => {
            let client = client(&config)?;
            let course = canvas_api::resolve_course(&client, &course).await?;
            let quizzes = client.quizzes(course.id).await?;
            if cli.json {
                print_json(&quizzes);
            } else if quizzes.is_empty() {
                out!("No classic quizzes (New Quizzes appear under `assignments`).");
            } else {
                for q in &quizzes {
                    let due = q
                        .due_at
                        .map(local_time)
                        .unwrap_or_else(|| "no due date".into());
                    out!("{due:<17} {}", q.title);
                }
            }
        }
        Command::Grades { course } => {
            let client = client(&config)?;
            match course {
                None => {
                    let grades = client.grades().await?;
                    if cli.json {
                        print_json(&grades);
                    } else {
                        for g in &grades {
                            let score = match (g.hidden, g.current_score) {
                                (true, _) => "hidden".to_owned(),
                                (false, Some(s)) => format!("{s:.1}%"),
                                (false, None) => "-".to_owned(),
                            };
                            out!("{score:>7}  {}", g.course);
                        }
                    }
                }
                Some(text) => {
                    let course = canvas_api::resolve_course(&client, &text).await?;
                    let grades = client.assignment_grades(course.id).await?;
                    if cli.json {
                        print_json(&grades);
                    } else {
                        for g in &grades {
                            let score = match (g.score, g.points_possible) {
                                (Some(s), Some(p)) => format!("{s}/{p}"),
                                (Some(s), None) => s.to_string(),
                                _ => "-".into(),
                            };
                            out!("{score:>10}  {}", g.name);
                        }
                    }
                }
            }
        }
        Command::Digest {
            since,
            upcoming,
            no_files,
            no_recordings,
        } => {
            let client = client(&config)?;
            let session = canvas_auth::load()?;
            let recap = panopto::Panopto::new(&config, &session)
                .ok()
                .map(|r| r.with_persistence(true));
            let now = chrono::Utc::now();
            let (since, from_marker) = canvas_digest::resolve_since(
                &since,
                &config.download_root,
                now,
                chrono::Duration::days(7),
            )?;
            let options = canvas_digest::Options {
                since,
                upcoming,
                files: !no_files,
                recordings: !no_recordings,
            };
            let (previous, note) = canvas_digest::load_state(&config.download_root, now);
            // Compare with the saved state only for scheduled (`last`) runs.
            let previous = previous.filter(|_| from_marker);
            let (mut d, state) =
                canvas_digest::digest(&client, recap.as_ref(), &options, previous.as_ref()).await?;
            d.skipped.extend(note);
            if cli.json {
                print_json(&d);
            } else {
                out!("Since {}", local_time(since));
                if d.no_changes() {
                    out!("Nothing new.");
                }
                for a in &d.new_announcements {
                    out!("announcement  {}", a.title);
                }
                for a in &d.new_assignments {
                    out!("new work      {}  {}", a.course, a.item.name);
                }
                for a in &d.changed_assignments {
                    out!("changed       {}  {}", a.course, a.item.name);
                }
                for f in &d.new_files {
                    out!("file          {}  {}", f.course, f.item.display_name);
                }
                for r in &d.new_recordings {
                    out!("recording     {}  {}", r.course, r.item.title);
                }
                if !d.upcoming.is_empty() {
                    out!("\nDue soon:");
                    for u in &d.upcoming {
                        let due = u.item.due_at.map(local_time).unwrap_or_default();
                        out!("  {due:<17} {}  {}", u.course, u.item.name);
                    }
                }
                for s in &d.skipped {
                    out!("not checked: {s}");
                }
            }
            // Only `--since last` advances the saved state, so ad-hoc
            // queries never hide changes from the next scheduled run. It is
            // saved after printing; a failure is a warning, not lost output.
            if from_marker && let Err(e) = canvas_digest::save_state(&config.download_root, &state)
            {
                eprintln!("warning: could not save the digest state: {e}");
            }
        }
        Command::Recap(RecapCommand::List { course }) => {
            let client = client(&config)?;
            let course = canvas_api::resolve_course(&client, &course).await?;
            let code = course
                .course_code
                .as_deref()
                .and_then(panopto::module_code)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "course {} has no module code to find its ReCap folder",
                        course.id
                    )
                })?;
            let session = canvas_auth::load()?;
            let recap = panopto::Panopto::new(&config, &session)?.with_persistence(true);
            let term = course.term.as_ref().map(|t| t.name.as_str());
            let result = recap.course_recordings(&code, term).await?;
            if cli.json {
                print_json(&result);
            } else {
                if let Some(w) = &result.warning {
                    eprintln!("warning: {w}");
                }
                if result.recordings.is_empty() {
                    out!("No recordings yet.");
                }
                for r in &result.recordings {
                    let when = r.start_time.map(local_time).unwrap_or_default();
                    let cc = if r.has_captions { "cc" } else { "  " };
                    out!(
                        "{when:<17} {:>4}m {cc}  {}  {}",
                        (r.duration_secs / 60.0).round(),
                        r.id,
                        r.title
                    );
                }
            }
        }
        Command::Recap(RecapCommand::Download {
            target,
            streams,
            no_mp4,
            latest,
            root,
        }) => {
            let session = canvas_auth::load()?;
            let recap = panopto::Panopto::new(&config, &session)?.with_persistence(true);
            let root = root.unwrap_or_else(|| config.download_root.clone());
            let options = panopto::DownloadOptions { streams, no_mp4 };
            let (dir, ids) = recap_targets(&config, &recap, &target, &root, latest).await?;
            let mut done = Vec::new();
            let mut failed: Vec<(String, String)> = Vec::new();
            for id in ids {
                let quiet = cli.json;
                let result = async {
                    let recording = recap.recording(&id).await?;
                    recap
                        .download(&recording, &dir, &options, |m| {
                            if !quiet {
                                eprintln!("{m}");
                            }
                        })
                        .await
                }
                .await;
                match result {
                    Ok(d) => done.push(d),
                    Err(e @ canvas_core::Error::SessionExpired) => return Err(e.into()),
                    Err(e) => failed.push((id, e.to_string())),
                }
            }
            if cli.json {
                let failed: Vec<_> = failed
                    .iter()
                    .map(|(id, e)| serde_json::json!({ "id": id, "error": e }))
                    .collect();
                print_json(&serde_json::json!({ "downloaded": done, "failed": failed }));
            } else {
                let written: usize = done.iter().map(|d| d.written.len()).sum();
                let skipped: usize = done.iter().map(|d| d.skipped.len()).sum();
                let bytes: u64 = done.iter().map(|d| d.bytes).sum();
                out!(
                    "{written} written ({}), {skipped} already there, {} failed",
                    human_bytes(bytes),
                    failed.len()
                );
                for (id, e) in &failed {
                    out!("  failed: {id} ({e})");
                }
                out!("Recordings are in {}", dir.display());
            }
            if !failed.is_empty() {
                anyhow::bail!("{} recording(s) failed to download", failed.len());
            }
        }
        Command::Recap(RecapCommand::Transcript {
            target,
            latest,
            stdout,
            language,
            root,
        }) => {
            let session = canvas_auth::load()?;
            let recap = panopto::Panopto::new(&config, &session)?.with_persistence(true);
            if stdout {
                let id = panopto::delivery_id(&target)?;
                let recording = recap.recording(&id).await?;
                let codes = panopto::languages(&recording, language)?;
                let mut all = Vec::new();
                for code in codes {
                    all.push(recap.transcript_in(&recording, code).await?);
                }
                if cli.json {
                    // One language keeps the original object shape.
                    match all.as_slice() {
                        [one] => print_json(one),
                        many => print_json(&many),
                    }
                } else {
                    for t in &all {
                        if all.len() > 1 {
                            out!("# Language {}\n", t.language);
                        }
                        out!("{}", t.text);
                    }
                }
                return Ok(());
            }
            let root = root.unwrap_or_else(|| config.download_root.clone());
            let (dir, ids) = recap_targets(&config, &recap, &target, &root, latest).await?;
            let mut saved: Vec<std::path::PathBuf> = Vec::new();
            let mut missing: Vec<String> = Vec::new();
            let mut failed: Vec<(String, String)> = Vec::new();
            for id in ids {
                // Look the recording up first, so a missing recording is a
                // failure, not "no captions".
                let recording = match recap.recording(&id).await {
                    Ok(r) => r,
                    Err(e @ canvas_core::Error::SessionExpired) => return Err(e.into()),
                    Err(e) => {
                        failed.push((id, e.to_string()));
                        continue;
                    }
                };
                match recap.save_transcripts(&recording, &dir, language).await {
                    Ok(report) => {
                        saved.extend(report.written);
                        for (code, e) in report.failed {
                            failed.push((format!("{id} (language {code})"), e));
                        }
                    }
                    Err(e @ canvas_core::Error::SessionExpired) => return Err(e.into()),
                    Err(canvas_core::Error::NotFound(_)) => missing.push(id),
                    Err(e) => failed.push((id, e.to_string())),
                }
            }
            if cli.json {
                let failed: Vec<_> = failed
                    .iter()
                    .map(|(id, e)| serde_json::json!({ "id": id, "error": e }))
                    .collect();
                print_json(
                    &serde_json::json!({ "saved": saved, "no_captions": missing, "failed": failed }),
                );
            } else {
                out!(
                    "{} file(s) saved, {} without captions, {} failed",
                    saved.len(),
                    missing.len(),
                    failed.len()
                );
                for (id, e) in &failed {
                    out!("  failed: {id} ({e})");
                }
                out!("Transcripts are in {}", dir.display());
            }
            if !failed.is_empty() {
                anyhow::bail!("{} transcript(s) failed", failed.len());
            }
        }
        Command::Recap(RecapCommand::Info { recording }) => {
            let session = canvas_auth::load()?;
            let recap = panopto::Panopto::new(&config, &session)?.with_persistence(true);
            let r = recap.recording(&recording).await?;
            if cli.json {
                print_json(&r);
            } else {
                out!("{}", r.title);
                if let Some(folder) = &r.folder {
                    out!("folder     {folder}");
                }
                if let Some(t) = r.start_time {
                    out!("recorded   {}", local_time(t));
                }
                out!("length     {}m", (r.duration_secs / 60.0).round());
                out!(
                    "mp4        {}",
                    if r.mp4_url.is_some() { "yes" } else { "no" }
                );
                let kinds: Vec<&str> = r.streams.iter().map(|s| s.kind.as_str()).collect();
                out!("streams    {}", kinds.join(", "));
                if r.has_captions {
                    let langs: Vec<String> =
                        r.caption_languages.iter().map(i64::to_string).collect();
                    out!(
                        "captions   yes (languages: {})",
                        if langs.is_empty() {
                            "1".to_owned()
                        } else {
                            langs.join(", ")
                        }
                    );
                } else {
                    out!("captions   no");
                }
                for c in &r.chapters {
                    out!("  {:>5}m  {}", (c.start_secs / 60.0).floor(), c.title);
                }
            }
        }
        Command::Files(FilesCommand::List { course }) => {
            let client = client(&config)?;
            let course = canvas_api::resolve_course(&client, &course).await?;
            let (files, failed) = canvas_sync::list_files(&client, &course).await?;
            if cli.json {
                print_json(&serde_json::json!({ "files": files, "failed": failed }));
            } else {
                for f in &files {
                    let lock = if f.locked_for_user { " (locked)" } else { "" };
                    out!("{:>10}  {}{lock}", human_bytes(f.size), f.display_name);
                }
                for f in &failed {
                    out!("    failed  {} ({})", f.name, f.reason);
                }
            }
            if !failed.is_empty() {
                anyhow::bail!("{} file(s) could not be read", failed.len());
            }
        }
        Command::Files(FilesCommand::Sync { course, all, root }) => {
            let client = client(&config)?;
            let courses = match (course, all) {
                (Some(text), _) => vec![canvas_api::resolve_course(&client, &text).await?],
                (None, true) => client.courses().await?,
                (None, false) => anyhow::bail!("name a course, or pass --all"),
            };
            let root = root.unwrap_or_else(|| config.download_root.clone());
            let mut reports = Vec::new();
            let mut course_errors: Vec<(u64, String)> = Vec::new();
            for course in &courses {
                let quiet = cli.json;
                let result = canvas_sync::sync_course(&client, course, &root, |path| {
                    if !quiet {
                        eprintln!("downloading {path}");
                    }
                })
                .await;
                match result {
                    Ok(report) => reports.push(report),
                    // An expired session fails every course; stop now.
                    Err(e @ canvas_core::Error::SessionExpired) => return Err(e.into()),
                    Err(e) => course_errors.push((course.id, e.to_string())),
                }
            }
            let failed: usize =
                reports.iter().map(|r| r.failed.len()).sum::<usize>() + course_errors.len();
            if cli.json {
                let errors: Vec<_> = course_errors
                    .iter()
                    .map(|(id, e)| serde_json::json!({ "course_id": id, "error": e }))
                    .collect();
                print_json(&serde_json::json!({ "courses": reports, "course_errors": errors }));
            } else {
                for r in &reports {
                    out!(
                        "course {}: {} downloaded ({}), {} unchanged, {} locked, {} failed",
                        r.course_id,
                        r.downloaded.len(),
                        human_bytes(r.bytes),
                        r.unchanged,
                        r.locked.len(),
                        r.failed.len()
                    );
                    for f in &r.failed {
                        out!("  failed: {} ({})", f.name, f.reason);
                    }
                    for w in &r.warnings {
                        out!("  warning: {w}");
                    }
                }
                for (id, e) in &course_errors {
                    out!("course {id}: not synced ({e})");
                }
                out!("Files are in {}", root.display());
            }
            if failed > 0 {
                anyhow::bail!("{failed} file(s) or course(s) failed to sync");
            }
        }
        Command::Discussions { course } => {
            let client = client(&config)?;
            let course = canvas_api::resolve_course(&client, &course).await?;
            let list = client.discussions(course.id).await?;
            if cli.json {
                print_json(&list);
            } else {
                for d in &list {
                    out!(
                        "{:>3} replies ({} unread)  {}",
                        d.replies,
                        d.unread,
                        d.title
                    );
                }
            }
        }
        Command::Mcp(McpCommand::Install { client }) => {
            let path = std::env::current_exe()?.with_file_name("canvas-mcp");
            if !path.exists() {
                eprintln!(
                    "warning: {} does not exist yet; run `make install` first",
                    path.display()
                );
            }
            let bin = path.display().to_string();
            // JSON string escaping is also valid as a TOML basic string.
            let quoted = serde_json::to_string(&bin)?;
            match client {
                McpClient::Claude => {
                    out!(
                        "claude mcp add --scope user canvas -- {}",
                        shell_quote(&bin)
                    )
                }
                McpClient::Codex => {
                    out!("Add to ~/.codex/config.toml:\n\n[mcp_servers.canvas]\ncommand = {quoted}")
                }
                McpClient::Cursor => out!(
                    "Add to ~/.cursor/mcp.json:\n\n{}",
                    serde_json::json!({ "mcpServers": { "canvas": { "command": bin } } })
                ),
            }
            eprintln!("canvas-mcp runs on the session from `canvas auth login`.");
        }
        Command::Courses => {
            let courses = client(&config)?.courses().await?;
            if cli.json {
                print_json(&courses);
            } else {
                for c in &courses {
                    let code = c.course_code.as_deref().unwrap_or("");
                    out!("{:<8} {:<26} {}", c.id, code, c.name);
                }
            }
        }
        Command::Assignments { due, course } => {
            let client = client(&config)?;
            let course = match course {
                Some(text) => Some(canvas_api::resolve_course(&client, &text).await?.id),
                None => None,
            };
            let list =
                canvas_api::upcoming_assignments(&client, due, chrono::Utc::now(), course).await?;
            if cli.json {
                print_json(&list);
            } else if list.is_empty() {
                out!("Nothing due.");
            } else {
                for u in &list {
                    let when = u
                        .assignment
                        .due_at
                        .map(|d| {
                            d.with_timezone(&chrono::Local)
                                .format("%a %d %b %H:%M")
                                .to_string()
                        })
                        .unwrap_or_default();
                    let done = if u.submitted { "submitted" } else { "" };
                    out!(
                        "{when:<17} {:<22} {:<10} {}",
                        u.course,
                        done,
                        u.assignment.name
                    );
                }
            }
        }
        Command::Auth(AuthCommand::Logout) => {
            canvas_auth::delete()?;
            if cli.json {
                print_json(&serde_json::json!({ "signed_in": false }));
            } else {
                out!("Session removed.");
            }
        }
    }
    Ok(())
}

/// Formats a byte count as B, KB, MB or GB.
fn human_bytes(n: u64) -> String {
    match n {
        n if n < 1_000 => format!("{n} B"),
        n if n < 1_000_000 => format!("{:.0} KB", n as f64 / 1e3),
        n if n < 1_000_000_000 => format!("{:.1} MB", n as f64 / 1e6),
        n => format!("{:.2} GB", n as f64 / 1e9),
    }
}

/// Formats a UTC time in the user's local zone for text output.
fn local_time(t: chrono::DateTime<chrono::Utc>) -> String {
    t.with_timezone(&chrono::Local)
        .format("%a %d %b %H:%M")
        .to_string()
}

/// Quotes `text` for a POSIX shell: wraps it in single quotes and escapes
/// any single quote inside.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', r"'\''"))
}

/// Resolves a `recap` target via [`panopto::recap_targets`], printing any
/// warning.
async fn recap_targets(
    config: &Config,
    recap: &panopto::Panopto,
    target: &str,
    root: &std::path::Path,
    latest: Option<u64>,
) -> anyhow::Result<(std::path::PathBuf, Vec<String>)> {
    let canvas = client(config)?;
    let latest = latest.map(|n| usize::try_from(n).unwrap_or(usize::MAX));
    let t = panopto::recap_targets(&canvas, recap, target, root, latest).await?;
    if let Some(w) = &t.warning {
        eprintln!("warning: {w}");
    }
    Ok((t.dir, t.ids))
}

/// Builds an API client from the stored session.
fn client(config: &Config) -> anyhow::Result<canvas_api::Client> {
    let session = canvas_auth::load()?;
    Ok(canvas_api::Client::new(config, &session)?)
}

fn print_json(value: &impl Serialize) {
    match serde_json::to_string(value) {
        Ok(s) => out!("{s}"),
        Err(e) => eprintln!("error: {e}"),
    }
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or_default()
}

fn human(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn ago(t: u64) -> String {
    format!("{} ago", human(now().saturating_sub(t)))
}

fn until(t: u64) -> String {
    human(t.saturating_sub(now()))
}

#[cfg(test)]
mod tests {
    use super::{clean, shell_quote};

    #[test]
    fn clean_strips_escape_codes_but_keeps_layout() {
        assert_eq!(clean("a\u{1b}[31mred\u{7}\tb\nc"), "a[31mred\tb\nc");
    }

    #[test]
    fn shell_quote_survives_spaces_dollars_and_quotes() {
        assert_eq!(shell_quote("/a b/c"), "'/a b/c'");
        assert_eq!(shell_quote("/x$y`z"), "'/x$y`z'");
        assert_eq!(shell_quote("/it's"), r"'/it'\''s'");
    }
}
