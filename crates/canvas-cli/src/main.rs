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
macro_rules! out {
    ($($arg:tt)*) => {
        if writeln!(std::io::stdout(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    };
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
        #[arg(long)]
        no_recap: bool,
        /// Seconds to wait for each sign-in step.
        #[arg(long, default_value_t = 300)]
        timeout: u64,
    },
    /// Check that the stored session still works.
    Status,
    /// Delete the stored session from the keychain.
    Logout,
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
        Command::Auth(AuthCommand::Login { no_recap, timeout }) => {
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
    use super::shell_quote;

    #[test]
    fn shell_quote_survives_spaces_dollars_and_quotes() {
        assert_eq!(shell_quote("/a b/c"), "'/a b/c'");
        assert_eq!(shell_quote("/x$y`z"), "'/x$y`z'");
        assert_eq!(shell_quote("/it's"), r"'/it'\''s'");
    }
}
