//! Saving Canvas wiki pages to disk as Markdown.
//!
//! Layout: `<root>/<course code>/<module>/<page title>.md`, using the first
//! module that links the page. Pages that no module links (found only in
//! the course page list) go in `<root>/<course code>/Pages/`.
//!
//! - **Sources.** The course page list when the user may see it, plus
//!   every `Page` item linked from a module. Many courses hide the page
//!   list from students, so modules are usually the only source.
//! - **Incremental.** Pages are small, so each is fetched every run and
//!   written only if the Markdown differs from the file on disk.
//! - **Atomic and contained.** As for file sync: written to a temp file
//!   under `.canvas-sync/tmp`, renamed into place, every name sanitised
//!   and the final path checked to be inside `root`.
//! - **Honest.** Pages that could not be saved are listed in
//!   [`PagesReport::failed`]; callers must not report success when it is
//!   non-empty.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use canvas_api::{Client, Course, Page};
use canvas_core::{Error, Result, sanitize};
use futures::StreamExt;
use serde::Serialize;

use crate::is_inside;

/// Folder for pages that no module links.
const UNFILED: &str = "Pages";

/// Page fetches in flight at once within one course.
const PAGE_CONCURRENCY: usize = 6;

/// What one page save did.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct PagesReport {
    /// Course id.
    pub course_id: u64,
    /// Pages written this run, as paths relative to the root.
    pub saved: Vec<String>,
    /// Pages whose file already matched.
    pub unchanged: usize,
    /// Pages that could not be saved, with the reason.
    pub failed: Vec<PageFailure>,
}

/// A page that could not be saved.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PageFailure {
    /// Page url slug.
    pub page: String,
    /// Title as listed, if known.
    pub title: String,
    /// Why it failed.
    pub reason: String,
}

/// A page to save: its slug, listed title and folder.
struct Wanted {
    slug: String,
    title: String,
    folder: String,
}

/// Saves a course's pages (or only the page `only`, by slug) under `root`
/// as Markdown.
///
/// `progress` is called with each relative path before it is written.
///
/// # Errors
/// Fails if neither the page list nor the modules can be read, or `root`
/// cannot be created. Per-page problems are reported in
/// [`PagesReport::failed`]; an expired session aborts with
/// [`Error::SessionExpired`].
pub async fn save_pages(
    client: &Client,
    course: &Course,
    root: &Path,
    only: Option<&str>,
    progress: impl Fn(&str),
) -> Result<PagesReport> {
    let io = |path: &Path| {
        let path = path.to_owned();
        move |source| Error::Io { path, source }
    };
    std::fs::create_dir_all(root).map_err(io(root))?;
    let root = root.canonicalize().map_err(io(root))?;
    let tmp_dir = root.join(".canvas-sync").join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(io(&tmp_dir))?;

    let mut wanted = collect(client, course).await?;
    if let Some(slug) = only {
        wanted.retain(|w| w.slug == slug);
        if wanted.is_empty() {
            // Not listed anywhere we can see; it may still be readable.
            wanted.push(Wanted {
                slug: slug.to_owned(),
                title: String::new(),
                folder: UNFILED.into(),
            });
        }
    }

    let fetched: Vec<(Wanted, Result<Page>)> = futures::stream::iter(wanted)
        .map(|w| async move {
            let page = client.page(course.id, &w.slug).await;
            (w, page)
        })
        .buffered(PAGE_CONCURRENCY)
        .collect()
        .await;

    let course_dir = sanitize(course.course_code.as_deref().unwrap_or(&course.name));
    let mut report = PagesReport {
        course_id: course.id,
        ..Default::default()
    };
    // Lowercased: macOS file systems ignore case by default.
    let mut taken: HashMap<String, String> = HashMap::new();
    for (w, result) in fetched {
        let page = match result {
            Ok(p) => p,
            Err(e @ Error::SessionExpired) => return Err(e),
            Err(e) => {
                report.failed.push(PageFailure {
                    page: w.slug,
                    title: w.title,
                    reason: e.to_string(),
                });
                continue;
            }
        };
        let title = if page.summary.title.is_empty() {
            &w.slug
        } else {
            &page.summary.title
        };
        let mut rel = PathBuf::from(&course_dir).join(sanitize(&w.folder));
        let key = |p: &Path| p.to_string_lossy().to_lowercase();
        let mut name = format!("{}.md", sanitize(title));
        if taken
            .get(&key(&rel.join(&name)))
            .is_some_and(|s| *s != w.slug)
        {
            name = format!("{} ({}).md", sanitize(title), w.slug);
        }
        rel.push(&name);
        taken.insert(key(&rel), w.slug.clone());
        let rel_text = rel.to_string_lossy().into_owned();
        let dest = root.join(&rel);
        if !is_inside(&root, &dest) {
            report.failed.push(PageFailure {
                page: w.slug,
                title: w.title,
                reason: "unsafe path".into(),
            });
            continue;
        }
        let text = render(&page);
        if std::fs::read_to_string(&dest).is_ok_and(|old| old == text) {
            report.unchanged += 1;
            continue;
        }
        progress(&rel_text);
        let part = tmp_dir.join(format!("page-{}-{}.part", course.id, report.saved.len()));
        let written = dest
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&part, &text))
            .and_then(|()| std::fs::rename(&part, &dest));
        match written {
            Ok(()) => report.saved.push(rel_text),
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                report.failed.push(PageFailure {
                    page: w.slug,
                    title: w.title,
                    reason: e.to_string(),
                });
            }
        }
    }
    Ok(report)
}

/// Lists every page the user can reach, each with the first module that
/// links it (or [`UNFILED`]), in module order then page-list order.
async fn collect(client: &Client, course: &Course) -> Result<Vec<Wanted>> {
    let modules = client.modules(course.id).await;
    let listed = client.pages(course.id).await;
    let hidden = |e: &Error| matches!(e, Error::Forbidden(_) | Error::NotFound(_));
    let (modules, listed) = match (modules, listed) {
        (Err(e), _) | (_, Err(e)) if !hidden(&e) => return Err(e),
        (Err(e), Err(_)) => return Err(e),
        (m, l) => (m.unwrap_or_default(), l.unwrap_or_default()),
    };
    let mut out: Vec<Wanted> = Vec::new();
    let mut push = |slug: String, title: String, folder: &str| {
        if !out.iter().any(|w| w.slug == slug) {
            out.push(Wanted {
                slug,
                title,
                folder: folder.to_owned(),
            });
        }
    };
    for module in modules {
        for item in module.items.unwrap_or_default() {
            if item.kind == "Page"
                && let Some(slug) = item.page_url
            {
                push(slug, item.title, &module.name);
            }
        }
    }
    for p in listed {
        push(p.url, p.title, UNFILED);
    }
    Ok(out)
}

/// The Markdown file for a page: title, source link and edit time, then
/// the body.
fn render(page: &Page) -> String {
    let mut text = format!("# {}\n\n", page.summary.title);
    if let Some(url) = &page.summary.html_url {
        text.push_str(&format!("Source: <{url}>\n"));
    }
    if let Some(t) = page.summary.updated_at {
        text.push_str(&format!("Updated: {}\n", t.format("%Y-%m-%d %H:%M UTC")));
    }
    text.push('\n');
    text.push_str(&page.body);
    text.push('\n');
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use canvas_api::PageSummary;

    #[test]
    fn render_has_title_source_and_body() {
        let page = Page {
            summary: PageSummary {
                url: "week-1".into(),
                title: "Week 1".into(),
                updated_at: None,
                html_url: Some("https://c/courses/1/pages/week-1".into()),
            },
            body: "Hello".into(),
            untrusted_text: true,
        };
        let text = render(&page);
        assert!(text.starts_with("# Week 1\n\n"));
        assert!(text.contains("Source: <https://c/courses/1/pages/week-1>"));
        assert!(text.ends_with("Hello\n"));
    }
}
