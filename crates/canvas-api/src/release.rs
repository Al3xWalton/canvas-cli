//! Checking GitHub for a newer canvas-cli.
//!
//! Compares the commit this build came from ([`canvas_core::build`])
//! with `main` on GitHub through the public compare API. No sign-in and no
//! Canvas session are needed, and nothing about the user is sent.

use canvas_core::build;
use serde::{Deserialize, Serialize};

/// Commits listed in a report, newest first, at most.
const MAX_CHANGES: usize = 30;

/// How the installed build compares with the newest on GitHub.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct UpdateStatus {
    /// Installed version line, e.g. `0.1.0 (1a2b3c4 2026-11-06)`.
    pub installed: String,
    /// Installed commit hash (empty if unknown).
    pub installed_commit: String,
    /// Newest commit on `main`, if GitHub could be reached.
    pub latest_commit: Option<String>,
    /// `Some(true)` if nothing newer exists, `None` if it could not be
    /// checked (see `error`).
    pub up_to_date: Option<bool>,
    /// Commits on `main` that the installed build lacks.
    pub behind_by: Option<u32>,
    /// Their summaries, newest first (at most 30).
    pub changes: Vec<String>,
    /// Folder the installed build was compiled from.
    pub source_dir: String,
    /// Why the check could not complete.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize)]
struct Compare {
    status: String,
    behind_by: u32,
    ahead_by: u32,
    #[serde(default)]
    commits: Vec<CompareCommit>,
}

#[derive(Deserialize)]
struct CompareCommit {
    sha: String,
    commit: CommitBody,
}

#[derive(Deserialize)]
struct CommitBody {
    message: String,
}

/// Asks GitHub whether a newer build exists. Never fails: problems are
/// reported in [`UpdateStatus::error`] with `up_to_date: None`.
pub async fn check_for_update() -> UpdateStatus {
    let mut status = UpdateStatus {
        installed: build::version_line(),
        installed_commit: build::COMMIT.to_owned(),
        latest_commit: None,
        up_to_date: None,
        behind_by: None,
        changes: Vec::new(),
        source_dir: build::SOURCE_DIR.to_owned(),
        error: None,
    };
    if build::COMMIT.is_empty() {
        status.error =
            Some("this build has no git commit recorded; reinstall from a git clone".into());
        return status;
    }
    match compare(build::COMMIT).await {
        Ok(c) => {
            // Our commit is the base, main is the head: `ahead_by` is how
            // far main is ahead of us. `diverged` means local commits too.
            let behind = c.ahead_by;
            status.latest_commit = c
                .commits
                .last()
                .map(|x| x.sha.clone())
                .or_else(|| (c.status == "identical").then(|| build::COMMIT.to_owned()));
            status.up_to_date = Some(behind == 0);
            status.behind_by = Some(behind);
            status.changes = c
                .commits
                .iter()
                .rev()
                .take(MAX_CHANGES)
                .map(|x| {
                    x.commit
                        .message
                        .lines()
                        .next()
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect();
            if c.behind_by > 0 {
                status.error = Some(format!(
                    "the installed build has {} commit(s) not on GitHub main (a local or unmerged build)",
                    c.behind_by
                ));
            }
        }
        Err(e) => status.error = Some(e),
    }
    status
}

async fn compare(commit: &str) -> Result<Compare, String> {
    let url = format!(
        "https://api.github.com/repos/{}/compare/{commit}...main",
        build::REPO
    );
    let http = reqwest::Client::builder()
        .user_agent(concat!("canvas-cli/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?;
    let resp = http
        .get(&url)
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("could not reach GitHub: {e}"))?;
    match resp.status().as_u16() {
        200 => resp
            .json()
            .await
            .map_err(|e| format!("unexpected answer from GitHub: {e}")),
        404 => Err("GitHub does not know this build's commit (a local, unpushed build)".into()),
        403 | 429 => Err("GitHub rate limit reached; try again later".into()),
        s => Err(format!("GitHub answered {s}")),
    }
}
