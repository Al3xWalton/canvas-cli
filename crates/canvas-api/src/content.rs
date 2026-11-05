//! Course content: modules, pages, announcements and discussions.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use canvas_core::{Error, Result};

use crate::{Client, html_to_markdown};

/// Reads a string that Canvas may send as `null`, as an empty string.
fn null_as_empty<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(Option::<String>::deserialize(d)?.unwrap_or_default())
}

/// Returns true for a Canvas page slug: ASCII letters, digits, `-`, `_`
/// and `.`, starting with a letter or digit. This rules out `.`, `..`,
/// `/`, `?` and `%`, so a slug can never change the request path.
fn valid_slug(slug: &str) -> bool {
    slug.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && slug
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// Courses per announcements request, keeping the URL short.
const ANNOUNCEMENT_BATCH: usize = 15;

/// One entry in a module: a page, file, assignment, link, and so on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModuleItem {
    /// Item id.
    pub id: u64,
    /// Title shown in the module (empty if Canvas sends none).
    #[serde(default, deserialize_with = "null_as_empty")]
    pub title: String,
    /// Kind: `Page`, `File`, `Assignment`, `Quiz`, `ExternalUrl`,
    /// `ExternalTool`, `SubHeader` or `Discussion`.
    #[serde(rename = "type")]
    pub kind: String,
    /// Link to the item in the Canvas web UI.
    #[serde(default)]
    pub html_url: Option<String>,
    /// For `ExternalUrl` and `ExternalTool` items, the target URL.
    #[serde(default)]
    pub external_url: Option<String>,
    /// For `Page` items, the page's url slug.
    #[serde(default)]
    pub page_url: Option<String>,
    /// For `File`, `Assignment`, `Quiz` and `Discussion` items, the id of
    /// the underlying object.
    #[serde(default)]
    pub content_id: Option<u64>,
}

/// A course module with its items.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Module {
    /// Module id.
    pub id: u64,
    /// Module name, e.g. `Week 3: Marketing mix`.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub name: String,
    /// Order within the course.
    #[serde(default)]
    pub position: u32,
    /// When the module unlocks, if scheduled.
    #[serde(default)]
    pub unlock_at: Option<DateTime<Utc>>,
    /// Items in order, or `None` if Canvas refused to list them (see
    /// `items_error`).
    #[serde(default)]
    pub items: Option<Vec<ModuleItem>>,
    /// Why the items could not be loaded, e.g. a locked module. When set,
    /// the module is not empty: its items are unknown.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub items_error: Option<String>,
    /// API URL for the items, used when Canvas omits them inline.
    #[serde(default, skip_serializing)]
    pub items_url: Option<String>,
}

/// A wiki page as listed, without its body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageSummary {
    /// The page's url slug, used to fetch it.
    pub url: String,
    /// Page title.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub title: String,
    /// Last edit time.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Link in the Canvas web UI.
    #[serde(default)]
    pub html_url: Option<String>,
}

/// A wiki page with its body as Markdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Page {
    /// Title, slug, times and link.
    #[serde(flatten)]
    pub summary: PageSummary,
    /// Body as Markdown. Third-party text.
    pub body: String,
    /// Always true: `title` and `body` are third-party text.
    pub untrusted_text: bool,
}

#[derive(Deserialize)]
struct RawPage {
    #[serde(flatten)]
    summary: PageSummary,
    #[serde(default)]
    body: Option<String>,
}

/// The author shown on an announcement or discussion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Author {
    /// Display name.
    #[serde(default)]
    pub display_name: Option<String>,
}

#[derive(Deserialize)]
struct RawTopic {
    id: u64,
    #[serde(default, deserialize_with = "null_as_empty")]
    title: String,
    #[serde(default)]
    message: Option<String>,
    #[serde(default)]
    posted_at: Option<DateTime<Utc>>,
    #[serde(default)]
    author: Option<Author>,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    context_code: Option<String>,
    #[serde(default)]
    discussion_subentry_count: u32,
    #[serde(default)]
    unread_count: u32,
}

/// A course announcement, body as Markdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Announcement {
    /// Announcement id.
    pub id: u64,
    /// Course id it was posted in.
    pub course_id: Option<u64>,
    /// Title. Third-party text.
    pub title: String,
    /// Body as Markdown. Third-party text.
    pub body: String,
    /// When it was posted.
    pub posted_at: Option<DateTime<Utc>>,
    /// Who posted it.
    pub author: Option<String>,
    /// Link in the Canvas web UI.
    pub html_url: Option<String>,
    /// Always true: `title` and `body` are third-party text.
    pub untrusted_text: bool,
}

/// A discussion topic, opening post as Markdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Discussion {
    /// Topic id.
    pub id: u64,
    /// Title. Third-party text.
    pub title: String,
    /// Opening post as Markdown. Third-party text.
    pub body: String,
    /// When it was posted.
    pub posted_at: Option<DateTime<Utc>>,
    /// Who started it.
    pub author: Option<String>,
    /// Number of replies.
    pub replies: u32,
    /// Replies the user has not read.
    pub unread: u32,
    /// Link in the Canvas web UI.
    pub html_url: Option<String>,
    /// Always true: `title` and `body` are third-party text.
    pub untrusted_text: bool,
}

impl Client {
    /// Lists a course's modules with their items, in order.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    pub async fn modules(&self, course_id: u64) -> Result<Vec<Module>> {
        let mut modules: Vec<Module> = self
            .get_all(&format!(
                "/api/v1/courses/{course_id}/modules?include[]=items&per_page=100"
            ))
            .await?;
        for module in &mut modules {
            if module.items.is_none() {
                // Canvas leaves items out of large modules; fetch them.
                let path = format!(
                    "/api/v1/courses/{course_id}/modules/{}/items?per_page=100",
                    module.id
                );
                match self.get_all(&path).await {
                    Ok(items) => module.items = Some(items),
                    Err(Error::NotFound(_)) => {
                        module.items_error = Some("Canvas hides this module's items".into());
                    }
                    Err(e) => return Err(e),
                }
            }
        }
        modules.sort_by_key(|m| m.position);
        Ok(modules)
    }

    /// Lists a course's wiki pages, newest edit first, without bodies.
    ///
    /// # Errors
    /// As for [`Client::get_all`]. Many courses hide the page list from
    /// students while still linking pages from modules; that 404 becomes
    /// [`Error::NotFound`] explaining where to find the pages instead.
    pub async fn pages(&self, course_id: u64) -> Result<Vec<PageSummary>> {
        self.get_all(&format!(
            "/api/v1/courses/{course_id}/pages?sort=updated_at&order=desc&per_page=100"
        ))
        .await
        .map_err(|e| match e {
            Error::NotFound(_) => Error::NotFound(format!(
                "course {course_id} hides its page list; its pages are listed as `Page` items \
                 in `modules` (use their `page_url`)"
            )),
            other => other,
        })
    }

    /// Fetches one page by its url slug, with its body as Markdown.
    ///
    /// # Errors
    /// As for [`Client::get`].
    pub async fn page(&self, course_id: u64, slug: &str) -> Result<Page> {
        let valid = valid_slug(slug);
        if !valid {
            return Err(Error::Input(format!(
                "`{slug}` is not a page slug; use the `url` from the page list or a module item's `page_url`"
            )));
        }
        let raw: RawPage = self
            .get(&format!("/api/v1/courses/{course_id}/pages/{slug}"))
            .await?;
        Ok(Page {
            summary: raw.summary,
            body: html_to_markdown(raw.body.as_deref().unwrap_or_default()),
            untrusted_text: true,
        })
    }

    /// Lists announcements posted in `course_ids` since `since`, newest
    /// first.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    pub async fn announcements(
        &self,
        course_ids: &[u64],
        since: DateTime<Utc>,
    ) -> Result<Vec<Announcement>> {
        if course_ids.is_empty() {
            return Ok(Vec::new());
        }
        let start = since.format("%Y-%m-%dT%H:%M:%SZ");
        // Canvas defaults end_date to 28 days after start_date, so always
        // send "now" or long windows lose the most recent posts.
        let end = Utc::now().format("%Y-%m-%dT%H:%M:%SZ");
        let mut raw: Vec<RawTopic> = Vec::new();
        for batch in course_ids.chunks(ANNOUNCEMENT_BATCH) {
            let contexts: String = batch
                .iter()
                .map(|id| format!("&context_codes[]=course_{id}"))
                .collect();
            raw.extend(
                self.get_all::<RawTopic>(&format!(
                    "/api/v1/announcements?per_page=100&start_date={start}&end_date={end}{contexts}"
                ))
                .await?,
            );
        }
        let mut out: Vec<Announcement> = raw
            .into_iter()
            .map(|t| Announcement {
                id: t.id,
                course_id: t
                    .context_code
                    .as_deref()
                    .and_then(|c| c.strip_prefix("course_"))
                    .and_then(|c| c.parse().ok()),
                title: t.title,
                body: html_to_markdown(t.message.as_deref().unwrap_or_default()),
                posted_at: t.posted_at,
                author: t.author.and_then(|a| a.display_name),
                html_url: t.html_url,
                untrusted_text: true,
            })
            .collect();
        out.sort_by_key(|a| std::cmp::Reverse(a.posted_at));
        Ok(out)
    }

    /// Lists a course's discussion topics, most recent activity first.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    pub async fn discussions(&self, course_id: u64) -> Result<Vec<Discussion>> {
        let raw: Vec<RawTopic> = self
            .get_all(&format!(
                "/api/v1/courses/{course_id}/discussion_topics?per_page=100&order_by=recent_activity"
            ))
            .await?;
        Ok(raw
            .into_iter()
            .map(|t| Discussion {
                id: t.id,
                title: t.title,
                body: html_to_markdown(t.message.as_deref().unwrap_or_default()),
                posted_at: t.posted_at,
                author: t.author.and_then(|a| a.display_name),
                replies: t.discussion_subentry_count,
                unread: t.unread_count,
                html_url: t.html_url,
                untrusted_text: true,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_cannot_change_the_path() {
        assert!(valid_slug("module-information-2"));
        assert!(valid_slug("week_1.notes"));
        for bad in ["", ".", "..", "../x", "a/b", "a?b=1", "%2e%2e", "a b", "-x"] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }

    #[test]
    fn null_titles_decode_as_empty() {
        let item: ModuleItem =
            serde_json::from_str(r#"{"id":1,"title":null,"type":"SubHeader"}"#).unwrap();
        assert_eq!(item.title, "");
        let item: ModuleItem = serde_json::from_str(r#"{"id":2,"type":"ExternalTool"}"#).unwrap();
        assert_eq!(item.title, "");
        let topic: RawTopic = serde_json::from_str(r#"{"id":3,"title":null}"#).unwrap();
        assert_eq!(topic.title, "");
    }
}
