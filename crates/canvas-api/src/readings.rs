//! Course readings, grouped by lecture.
//!
//! Universities keep readings in different places, so this module
//! gathers them from all of them and says which it could not read:
//!
//! - **Canvas.** Module items whose title names a reading (`Pre-reading`,
//!   `Additional reading`, `Textbook`, `Preparation`...), and every item
//!   under a sub-heading that does. Pages are fetched and each link in
//!   them becomes an item; the page text is kept too, because lecturers
//!   often write citations as plain text.
//! - **Talis Aspire.** Lists linked from Canvas are read through Talis's
//!   public linked-data JSON (`/lists/<id>.json`), section by section.
//!   No sign-in is needed.
//! - **Everything else** (Leganto, other systems, course-menu tools) is
//!   returned in [`Readings::lists`] with its link and the reason it was
//!   not read, so an agent can tell the user where to look.
//!
//! Lecture grouping is by module, then sub-heading, which is how courses
//! lay out weeks and lectures. Every title and body is third-party text.

use std::collections::{BTreeMap, HashMap};
use std::sync::LazyLock;

use futures::StreamExt;
use regex::Regex;
use serde::Serialize;
use serde_json::Value;

use canvas_core::{Error, Result};

use crate::{Client, Module, ModuleItem, Page};

/// Words that mark a module item or sub-heading as reading material.
const READING_WORDS: &[&str] = &[
    "reading",
    "pre-read",
    "preread",
    "textbook",
    "text book",
    "bibliography",
    "literature",
    "resource list",
    "set text",
    "core text",
    "preparation",
    "prepare for",
    "pre-lecture",
    "pre lecture",
    "pre-seminar",
    "pre-class",
    "pre-workshop",
    "case study",
    "learning materials",
    "resources",
];

/// Words that mark an item as assessment rather than reading, unless its
/// title also says `read`.
const ASSESSMENT_WORDS: &[&str] = &["quiz", "exam", "assessment", "answers", "submission"];

/// Pages fetched at once.
const PAGE_CONCURRENCY: usize = 6;

/// Talis requests at once.
const TALIS_CONCURRENCY: usize = 8;

/// Most Talis items read from one list, as a guard.
const MAX_TALIS_ITEMS: usize = 600;

/// One reading: a book, article, chapter, link, video or file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Reading {
    /// Title or link text.
    pub title: String,
    /// `book`, `chapter`, `article`, `journal`, `video`, `web`,
    /// `library` (a library catalogue record), `file`, `page` (another
    /// Canvas page), `note` (the
    /// lecturer's words, in `note`) or `unknown`.
    pub kind: String,
    /// Authors, when the source lists them.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub authors: Vec<String>,
    /// Where it appeared: journal or book title, edition, volume, pages,
    /// date, publisher, as one line.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub details: String,
    /// `essential`, `recommended`, `further` and so on, when the list says.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub importance: Option<String>,
    /// Link to the item (online copy, catalogue record or web page).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// DOI, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub doi: Option<String>,
    /// ISBN or ISSN, if known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub isbn: Option<String>,
    /// For Canvas files, the file id (download with `files sync`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_id: Option<u64>,
    /// Lecturer's note on the item.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub note: String,
}

/// Readings for one lecture, week or list section.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ReadingSection {
    /// Module (week or topic) name, or the list's name.
    pub module: String,
    /// Sub-heading within the module or list section, e.g. `Week 5:
    /// Digital infrastructure` (empty if none).
    pub heading: String,
    /// The page or item the readings came from, e.g. `Pre-lecture case
    /// study` (empty for a group of files and links).
    #[serde(skip_serializing_if = "String::is_empty")]
    pub title: String,
    /// `canvas` or `talis`.
    pub source: String,
    /// Canvas page slug the section came from, if any (for `read_page`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
    /// The page text as Markdown, which often holds plain-text citations.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub text: String,
    /// The items.
    pub items: Vec<Reading>,
}

/// A reading list in another system.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExternalList {
    /// `talis`, `leganto`, `course-tool` or `other`.
    pub system: String,
    /// Its link.
    pub url: String,
    /// Where in Canvas it was found.
    pub found_in: String,
    /// `read` if its items are in [`Readings::sections`], otherwise why
    /// not.
    pub status: String,
}

/// Everything found for one course.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Readings {
    /// Course id.
    pub course_id: u64,
    /// Sections in course order: Canvas modules first, then lists.
    pub sections: Vec<ReadingSection>,
    /// Reading lists in other systems, read or not.
    pub lists: Vec<ExternalList>,
    /// Things that went wrong without failing the whole call.
    pub warnings: Vec<String>,
    /// Always true: titles, notes and text are third-party.
    pub untrusted_text: bool,
}

/// True if `text` names reading material.
pub fn is_reading_title(text: &str) -> bool {
    let lower = text.to_lowercase();
    READING_WORDS.iter().any(|w| lower.contains(w))
}

/// True if `text` names assessment and not reading.
fn is_assessment_title(text: &str) -> bool {
    let lower = text.to_lowercase();
    !lower.contains("read") && ASSESSMENT_WORDS.iter().any(|w| lower.contains(w))
}

/// A module item chosen as reading, with its heading.
#[derive(Debug, Clone)]
struct Picked {
    module: String,
    heading: String,
    item: ModuleItem,
}

/// Chooses the reading items in `modules`: items whose title names a
/// reading, and every item under a sub-heading that does. A module whose
/// own name names a reading (e.g. `Reading list`) counts as a heading.
fn pick(modules: &[Module]) -> Vec<Picked> {
    let mut out = Vec::new();
    for m in modules {
        let module_is_reading = is_reading_title(&m.name);
        let mut heading = String::new();
        let mut under_reading = module_is_reading;
        for item in m.items.iter().flatten() {
            if item.kind == "SubHeader" {
                heading = item.title.clone();
                under_reading = module_is_reading || is_reading_title(&item.title);
                continue;
            }
            if (under_reading || is_reading_title(&item.title)) && !is_assessment_title(&item.title)
            {
                out.push(Picked {
                    module: m.name.clone(),
                    heading: heading.clone(),
                    item: item.clone(),
                });
            }
        }
    }
    out
}

/// Markdown links `[text](url)`, skipping images.
static MD_LINK: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"(!?)\[([^\]]*)\]\(<?([^)\s>]+)>?(?:\s+"[^"]*")?\)"#).ok());

/// A linked image, `[![alt](img)](target)`, flattened to `[alt](target)`.
static LINKED_IMAGE: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"\[!\[([^\]]*)\]\([^)]*\)\]\("#).ok());

/// Bare URLs in text.
static BARE_URL: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"https?://[^\s)<>\]"']+"#).ok());

/// A DOI anywhere in a URL or text.
static DOI: LazyLock<Option<Regex>> =
    LazyLock::new(|| Regex::new(r#"\b(10\.\d{4,9}/[^\s"<>]+)"#).ok());

/// An ISBN-13 or ISBN-10 after the word ISBN.
static ISBN: LazyLock<Option<Regex>> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\bISBN(?:-1[03])?:?\s*((?:97[89][- ]?)?\d{1,5}[- ]?\d{1,7}[- ]?\d{1,7}[- ]?[\dX])\b",
    )
    .ok()
});

/// Guesses an item's kind from its link and title.
pub fn classify(url: &str, title: &str) -> &'static str {
    let u = url.to_lowercase();
    let t = title.to_lowercase();
    if [
        "youtube.com",
        "youtu.be",
        "vimeo.com",
        "panopto",
        "/embed",
        "bbc.co.uk/iplayer",
        "box of broadcasts",
        "learningonscreen",
        "iptv.",
        "kaltura",
        "mediaspace",
        "stream.",
        "/video",
        "ted.com",
    ]
    .iter()
    .any(|h| u.contains(h))
    {
        "video"
    } else if u.contains("doi.org/")
        || u.contains("jstor.org")
        || u.contains("sciencedirect.com")
        || u.contains("springer.com")
        || u.contains("wiley.com")
        || u.contains("tandfonline.com")
        || u.contains("sagepub.com")
        || u.contains("emerald.com")
        || u.contains("ft.com")
        || u.contains("hbr.org")
    {
        "article"
    } else if u.contains("uresolver")
        || u.contains("primo")
        || u.contains("libsearch")
        || u.contains("library")
        || u.contains("vlebooks")
        || u.contains("ebookcentral")
        || u.contains("dawsonera")
        || u.contains("kortext")
        || u.contains("worldcat")
    {
        if t.contains("chapter") {
            "chapter"
        } else {
            "library"
        }
    } else if u.contains("/files/") && u.contains("/courses/") {
        "file"
    } else if u.contains("/courses/") && u.contains("/pages/") {
        "page"
    } else if t.contains("chapter") {
        "chapter"
    } else if u.starts_with("http") {
        "web"
    } else {
        "unknown"
    }
}

/// What kind of reading-list system a link belongs to, if any.
pub fn list_system(url: &str) -> Option<&'static str> {
    let u = url.to_lowercase();
    if u.contains(".rl.talis.com") || u.contains("talis") && talis_list_id(url).is_some() {
        Some("talis")
    } else if u.contains("leganto") {
        Some("leganto")
    } else if u.contains("readinglist")
        || u.contains("reading-list")
        || u.contains("resourcelist")
        || u.contains("rebus")
        || u.contains("keylinks")
        || u.contains("rl.talis")
    {
        Some("other")
    } else {
        None
    }
}

/// Extracts the Talis list id (a GUID) from a list URL.
pub fn talis_list_id(url: &str) -> Option<String> {
    static ID: LazyLock<Option<Regex>> = LazyLock::new(|| {
        Regex::new(r"(?i)/lists/([0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12})")
            .ok()
    });
    ID.as_ref()?
        .captures(url)
        .and_then(|c| c.get(1))
        .map(|m| m.as_str().to_uppercase())
}

/// The importance a phrase gives a reading: `essential`, `recommended`,
/// `optional` or `further`, or `None` if it says nothing. When several
/// apply (`optional/recommended`) the strongest wins.
pub fn importance_in(text: &str) -> Option<&'static str> {
    let t = text.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if has(&[
        "essential",
        "required",
        "compulsory",
        "mandatory",
        "core read",
        "core text",
        "must read",
        "must-read",
        "key read",
        "key text",
        "set text",
    ]) {
        Some("essential")
    } else if has(&["recommended", "suggested", "highly advised"]) {
        Some("recommended")
    } else if has(&["optional", "if you have time", "if interested"]) {
        Some("optional")
    } else if has(&[
        "further read",
        "additional read",
        "wider read",
        "extra read",
        "background read",
        "supplementary",
        "going further",
    ]) {
        // "Textbook and additional readings" covers both: say nothing.
        (!has(&["textbook", "core"])).then_some("further")
    } else {
        None
    }
}

/// The text of a Markdown heading line (`## Week 5`, or a line that is
/// all bold), without its markup.
fn heading_text(line: &str) -> Option<String> {
    let l = line.trim();
    let text = if l.starts_with('#') {
        l.trim_start_matches('#')
    } else if l.len() > 4 && l.starts_with("**") && l.ends_with("**") && !l.contains("](") {
        &l[2..l.len() - 2]
    } else {
        return None;
    };
    let text = text.replace('\\', "");
    let text = text.trim().trim_matches(['*', '_', ':']).trim();
    (!text.is_empty()).then(|| text.to_owned())
}

/// Turns a page's Markdown into reading items, one per distinct link.
/// Images, mail links and in-page anchors are skipped.
///
/// Each link's importance comes from its own line, else the heading
/// above it. A link whose text is empty or the bare URL takes the nearest
/// heading as its title.
pub fn links_in(markdown: &str) -> Vec<Reading> {
    let markdown = match LINKED_IMAGE.as_ref() {
        Some(re) => re.replace_all(markdown, "[$1](").into_owned(),
        None => markdown.to_owned(),
    };
    let mut out: Vec<Reading> = Vec::new();
    let mut heading: Option<String> = None;
    let mut heading_importance: Option<&'static str> = None;
    // Links seen before the first heading after them, whose title is
    // still their URL: a heading just below a bare link often names it.
    let mut untitled: Vec<usize> = Vec::new();
    for line in markdown.lines() {
        if let Some(h) = heading_text(line) {
            heading_importance = importance_in(&h);
            for &i in &untitled {
                if let Some(r) = out.get_mut(i)
                    && r.url.as_deref() == Some(r.title.as_str())
                {
                    r.title.clone_from(&h);
                }
            }
            untitled.clear();
            heading = Some(h);
            continue;
        }
        let mut found: Vec<(String, String)> = Vec::new();
        let mut rest = line.to_owned();
        if let Some(re) = MD_LINK.as_ref() {
            for c in re.captures_iter(line) {
                if c.get(1).is_some_and(|m| m.as_str() == "!") {
                    continue;
                }
                let text = c.get(2).map_or("", |m| m.as_str());
                found.push((
                    text.to_owned(),
                    c.get(3).map_or("", |m| m.as_str()).to_owned(),
                ));
            }
            rest = re.replace_all(line, " ").into_owned();
        }
        if let Some(re) = BARE_URL.as_ref() {
            for m in re.find_iter(&rest) {
                found.push((String::new(), m.as_str().to_owned()));
            }
        }
        if found.is_empty() {
            continue;
        }
        let line_words = BARE_URL.as_ref().map_or_else(
            || line.to_owned(),
            |re| re.replace_all(line, " ").into_owned(),
        );
        let importance = importance_in(&line_words).or(heading_importance);
        for (text, url) in found {
            let url = url.trim_end_matches(['.', ',', ';']);
            if !url.starts_with("http") {
                continue;
            }
            let title = text.replace('\\', "");
            let title = title.trim().trim_matches(['*', '_']).trim();
            let bare = title.is_empty() || title == url;
            if let Some(i) = out.iter().position(|r| r.url.as_deref() == Some(url)) {
                // Seen before: keep the first, but take a better title.
                if let Some(r) = out.get_mut(i) {
                    if r.title == url {
                        if !bare {
                            r.title = title.to_owned();
                        } else if let Some(h) = &heading {
                            r.title.clone_from(h);
                        }
                    }
                    if r.importance.is_none() {
                        r.importance = importance.map(str::to_owned);
                    }
                }
                continue;
            }
            let title = if bare { url } else { title };
            if bare {
                untitled.push(out.len());
            }
            out.push(Reading {
                title: title.to_owned(),
                file_id: canvas_file_id(url),
                kind: classify(url, title).to_owned(),
                url: Some(url.to_owned()),
                doi: DOI
                    .as_ref()
                    .and_then(|re| re.captures(url))
                    .and_then(|c| c.get(1))
                    .map(|m| m.as_str().to_owned()),
                importance: importance.map(str::to_owned),
                ..Default::default()
            });
        }
    }
    out
}

/// The file id in a Canvas file link (`.../courses/1/files/42?wrap=1`).
fn canvas_file_id(url: &str) -> Option<u64> {
    if !url.contains("/courses/") {
        return None;
    }
    let after = url.split("/files/").nth(1)?;
    after
        .split(|c: char| !c.is_ascii_digit())
        .next()?
        .parse()
        .ok()
}

/// ISBNs written in text, e.g. a textbook citation.
pub fn isbns_in(text: &str) -> Vec<String> {
    ISBN.as_ref().map_or_else(Vec::new, |re| {
        re.captures_iter(text)
            .filter_map(|c| c.get(1))
            .map(|m| m.as_str().replace([' ', '-'], ""))
            .collect()
    })
}

/// Builds the Canvas sections from picked items and the fetched pages.
fn canvas_sections(picked: &[Picked], pages: &HashMap<String, Page>) -> Vec<ReadingSection> {
    let mut sections: Vec<ReadingSection> = Vec::new();
    for p in picked {
        let item = &p.item;
        let mut section = ReadingSection {
            module: p.module.clone(),
            heading: p.heading.clone(),
            source: "canvas".into(),
            ..Default::default()
        };
        match item.kind.as_str() {
            "Page" => {
                let Some(slug) = &item.page_url else { continue };
                section.page = Some(slug.clone());
                match pages.get(slug) {
                    Some(page) => {
                        section.text = page.body.clone();
                        section.items = links_in(&page.body);
                        for isbn in isbns_in(&page.body) {
                            section.items.push(Reading {
                                title: format!("ISBN {isbn} (cited in the page text)"),
                                kind: "book".into(),
                                isbn: Some(isbn),
                                ..Default::default()
                            });
                        }
                        if section.items.is_empty() && section.text.is_empty() {
                            continue;
                        }
                        section.title = if page.summary.title.is_empty() {
                            item.title.clone()
                        } else {
                            page.summary.title.clone()
                        };
                    }
                    None => {
                        section.title.clone_from(&item.title);
                        section.items.push(Reading {
                            title: item.title.clone(),
                            kind: "unknown".into(),
                            url: item.html_url.clone(),
                            note: "page could not be read".into(),
                            ..Default::default()
                        });
                    }
                }
            }
            "File" => section.items.push(Reading {
                title: item.title.clone(),
                kind: "file".into(),
                url: item.html_url.clone(),
                file_id: item.content_id,
                ..Default::default()
            }),
            "ExternalUrl" | "ExternalTool" => {
                let url = item.external_url.clone().or_else(|| item.html_url.clone());
                let kind = url.as_deref().map_or("web", |u| classify(u, &item.title));
                section.items.push(Reading {
                    title: item.title.clone(),
                    kind: kind.into(),
                    doi: url.as_deref().and_then(|u| {
                        DOI.as_ref()
                            .and_then(|re| re.captures(u))
                            .and_then(|c| c.get(1))
                            .map(|m| m.as_str().to_owned())
                    }),
                    url,
                    ..Default::default()
                });
            }
            "Assignment" | "Quiz" | "Discussion" => continue,
            _ => section.items.push(Reading {
                title: item.title.clone(),
                kind: "unknown".into(),
                url: item.html_url.clone(),
                ..Default::default()
            }),
        }
        // Items the page did not mark take the page title, item title or
        // module heading's word for it.
        let context = importance_in(&section.title)
            .or_else(|| importance_in(&item.title))
            .or_else(|| importance_in(&section.heading));
        for r in &mut section.items {
            if r.importance.is_none() {
                r.importance = context.map(str::to_owned);
            }
        }
        // Merge consecutive items of the same heading into one section.
        match sections.last_mut() {
            Some(last)
                if last.module == section.module
                    && last.heading == section.heading
                    && section.page.is_none()
                    && last.page.is_none() =>
            {
                last.items.extend(section.items);
            }
            _ => sections.push(section),
        }
    }
    sections
}

/// The reading pages (slugs) among `modules`, to fetch before calling
/// [`from_canvas`].
pub fn reading_pages(modules: &[Module]) -> Vec<String> {
    let mut slugs: Vec<String> = Vec::new();
    for p in pick(modules) {
        if p.item.kind == "Page"
            && let Some(slug) = p.item.page_url
            && !slugs.contains(&slug)
        {
            slugs.push(slug);
        }
    }
    slugs
}

/// Builds the Canvas sections and the reading lists they link from
/// already-fetched data: the course's modules, its reading pages by slug
/// (see [`reading_pages`]) and its tabs (`/courses/:id/tabs`). Lists are
/// returned unread, with an empty `status`.
pub fn from_canvas(
    modules: &[Module],
    pages: &HashMap<String, Page>,
    tabs: &[Value],
) -> (Vec<ReadingSection>, Vec<ExternalList>) {
    let sections = canvas_sections(&pick(modules), pages);
    let lists = find_lists(&sections, tabs);
    (sections, lists)
}

/// Finds reading-list links in the Canvas sections, and course-menu tabs
/// that look like reading lists.
fn find_lists(sections: &[ReadingSection], tabs: &[Value]) -> Vec<ExternalList> {
    let mut out: Vec<ExternalList> = Vec::new();
    for s in sections {
        for r in &s.items {
            let Some(url) = &r.url else { continue };
            let Some(system) = list_system(url) else {
                continue;
            };
            if out.iter().any(|l| &l.url == url) {
                continue;
            }
            out.push(ExternalList {
                system: system.into(),
                url: url.clone(),
                found_in: format!("{} / {}", s.module, s.heading),
                status: String::new(),
            });
        }
    }
    for tab in tabs {
        let label = tab["label"].as_str().unwrap_or_default();
        let lower = label.to_lowercase();
        let looks = lower.contains("reading")
            || lower.contains("resource list")
            || lower.contains("leganto")
            || lower.contains("talis");
        if !looks {
            continue;
        }
        let url = tab["full_url"]
            .as_str()
            .or_else(|| tab["html_url"].as_str())
            .unwrap_or_default()
            .to_owned();
        let system = if lower.contains("leganto") {
            "leganto"
        } else if lower.contains("talis") {
            "talis"
        } else {
            "course-tool"
        };
        out.push(ExternalList {
            system: system.into(),
            url,
            found_in: format!("course menu: {label}"),
            status: "opens through a Canvas tool (LTI) that needs a browser sign-in; open the link in Canvas".into(),
        });
    }
    out
}

impl Client {
    /// Gathers a course's readings from Canvas and the reading lists it
    /// links, grouped by lecture.
    ///
    /// `lecture`, if given, keeps only sections whose module or heading
    /// contains it (any case), e.g. `week 3` or `seminar 2`.
    ///
    /// # Errors
    /// Fails if the modules cannot be read or the session has expired.
    /// Pages and lists that cannot be read are reported in the result.
    pub async fn readings(&self, course_id: u64, lecture: Option<&str>) -> Result<Readings> {
        let mut out = Readings {
            course_id,
            untrusted_text: true,
            ..Default::default()
        };
        let modules = match self.modules(course_id).await {
            Ok(m) => m,
            Err(Error::Forbidden(_) | Error::NotFound(_)) => {
                out.warnings.push(
                    "this course hides its modules; no Canvas readings could be found".into(),
                );
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        let slugs = reading_pages(&modules);
        let fetched: Vec<(String, Result<Page>)> = futures::stream::iter(slugs)
            .map(|slug| async move {
                let page = self.page(course_id, &slug).await;
                (slug, page)
            })
            .buffered(PAGE_CONCURRENCY)
            .collect()
            .await;
        let mut pages = HashMap::new();
        for (slug, result) in fetched {
            match result {
                Ok(p) => {
                    pages.insert(slug, p);
                }
                Err(e @ Error::SessionExpired) => return Err(e),
                Err(e) => out.warnings.push(format!("page {slug}: {e}")),
            }
        }

        let tabs: Vec<Value> = match self
            .get::<Vec<Value>>(&format!("/api/v1/courses/{course_id}/tabs"))
            .await
        {
            Ok(t) => t,
            Err(Error::SessionExpired) => return Err(Error::SessionExpired),
            Err(Error::Forbidden(_) | Error::NotFound(_)) => Vec::new(),
            Err(e) => {
                out.warnings.push(format!("course menu not checked: {e}"));
                Vec::new()
            }
        };
        (out.sections, out.lists) = from_canvas(&modules, &pages, &tabs);

        for list in &mut out.lists {
            if !list.status.is_empty() {
                continue;
            }
            match list.system.as_str() {
                "talis" => match read_talis(&list.url).await {
                    Ok(mut sections) => {
                        for s in &mut sections {
                            if s.module.is_empty() {
                                s.module.clone_from(&list.found_in);
                            }
                        }
                        out.sections.extend(sections);
                        "read".clone_into(&mut list.status);
                    }
                    Err(e) => list.status = format!("could not read the Talis list: {e}"),
                },
                "leganto" => {
                    list.status = "Leganto lists need a university sign-in and are not read yet; open the link".into();
                }
                _ => list.status = "unknown reading-list system; open the link".into(),
            }
        }

        if let Some(filter) = lecture {
            let f = filter.to_lowercase();
            out.sections.retain(|s| {
                s.module.to_lowercase().contains(&f)
                    || s.heading.to_lowercase().contains(&f)
                    || s.title.to_lowercase().contains(&f)
            });
        }
        Ok(out)
    }
}

// ---------------------------------------------------------------- Talis

const RDF: &str = "http://www.w3.org/1999/02/22-rdf-syntax-ns#";

/// A graph from Talis's linked-data JSON: subject → predicate → values.
type Graph = BTreeMap<String, BTreeMap<String, Vec<Value>>>;

/// Values of `pred` (matched by its last segment after `/` or `#`).
fn values<'a>(graph: &'a Graph, subject: &str, pred: &str) -> Vec<&'a str> {
    graph
        .get(subject)
        .map(|p| {
            p.iter()
                .filter(|(k, _)| k.rsplit(['/', '#']).next() == Some(pred))
                .flat_map(|(_, v)| v.iter().filter_map(|x| x["value"].as_str()))
                .collect()
        })
        .unwrap_or_default()
}

/// The ordered members (`rdf:_1`, `rdf:_2`...) of a sequence.
fn members(graph: &Graph, subject: &str) -> Vec<String> {
    let mut m: Vec<(u32, String)> = graph
        .get(subject)
        .map(|p| {
            p.iter()
                .filter_map(|(k, v)| {
                    let n: u32 = k.strip_prefix(RDF)?.strip_prefix('_')?.parse().ok()?;
                    Some((n, v.first()?["value"].as_str()?.to_owned()))
                })
                .collect()
        })
        .unwrap_or_default();
    m.sort();
    m.into_iter().map(|(_, s)| s).collect()
}

/// Rewrites a Talis URI (which may use the university's own domain) onto
/// the host the list was linked from, adding `.json`.
fn talis_json_url(host_base: &str, uri: &str) -> Option<String> {
    let path = url::Url::parse(uri).ok()?.path().to_owned();
    Some(format!(
        "{host_base}{}.json",
        path.trim_end_matches(".html")
    ))
}

/// Reads a Talis Aspire list by its public URL, returning one section per
/// list section in order (items directly on the list come first).
///
/// # Errors
/// [`Error::Http`] or [`Error::Decode`] if the list cannot be read.
pub async fn read_talis(list_url: &str) -> Result<Vec<ReadingSection>> {
    let id = talis_list_id(list_url)
        .ok_or_else(|| Error::Input(format!("`{list_url}` is not a Talis list link")))?;
    let parsed = url::Url::parse(list_url).map_err(|e| Error::Input(e.to_string()))?;
    if parsed.scheme() != "https" {
        return Err(Error::Input("Talis links must use https".into()));
    }
    let base = format!("https://{}", parsed.host_str().unwrap_or_default());
    let http = reqwest::Client::builder()
        .user_agent(concat!("canvas-cli/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| Error::Http(e.to_string()))?;
    let fetch = |url: String| {
        let http = http.clone();
        async move {
            let resp = http
                .get(&url)
                .send()
                .await
                .map_err(|e| Error::Http(e.to_string()))?;
            if !resp.status().is_success() {
                return Err(Error::Http(format!("{url} answered {}", resp.status())));
            }
            resp.json::<Graph>().await.map_err(|e| Error::Decode {
                what: "Talis list".into(),
                detail: e.to_string(),
            })
        }
    };
    let list = fetch(format!("{base}/lists/{id}.json")).await?;
    let list_uri = list
        .keys()
        .find(|k| k.to_uppercase().ends_with(&id))
        .cloned()
        .ok_or_else(|| Error::Decode {
            what: "Talis list".into(),
            detail: "list not in its own JSON".into(),
        })?;
    let list_name = values(&list, &list_uri, "name")
        .first()
        .map_or_else(String::new, |s| (*s).to_owned());

    // Walk the list: members are sections (which hold items) or items.
    let mut order: Vec<(String, String)> = Vec::new(); // (heading, item uri)
    let mut descriptions: HashMap<String, String> = HashMap::new();
    let mut stack: Vec<(String, String)> = members(&list, &list_uri)
        .into_iter()
        .rev()
        .map(|m| (String::new(), m))
        .collect();
    let mut sections_seen = 0;
    while let Some((heading, uri)) = stack.pop() {
        if order.len() >= MAX_TALIS_ITEMS || sections_seen > 200 {
            break;
        }
        if uri.contains("/items/") {
            order.push((heading, uri));
        } else if uri.contains("/sections/") {
            sections_seen += 1;
            let Some(url) = talis_json_url(&base, &uri) else {
                continue;
            };
            let g = fetch(url).await?;
            let name = values(&g, &uri, "name")
                .first()
                .map_or_else(String::new, |s| s.trim().to_owned());
            let name = if heading.is_empty() {
                name
            } else {
                format!("{heading} / {name}")
            };
            if let Some(d) = values(&g, &uri, "description").first() {
                let d = crate::html_to_markdown(d);
                if !d.is_empty() {
                    descriptions.insert(name.clone(), d);
                }
            }
            for m in members(&g, &uri).into_iter().rev() {
                stack.push((name.clone(), m));
            }
        }
    }

    let items: Vec<(String, Result<Reading>)> = futures::stream::iter(order)
        .map(|(heading, uri)| {
            let base = base.clone();
            async move {
                let item = match talis_json_url(&base, &uri) {
                    Some(url) => fetch(url).await.map(|g| talis_item(&g, &uri, &base)),
                    None => Err(Error::Input(uri.clone())),
                };
                (heading, item)
            }
        })
        .buffered(TALIS_CONCURRENCY)
        .collect()
        .await;

    let mut sections: Vec<ReadingSection> = Vec::new();
    for (heading, item) in items {
        let reading = match item {
            Ok(r) => r,
            Err(e) => Reading {
                title: "item could not be read".into(),
                kind: "unknown".into(),
                note: e.to_string(),
                ..Default::default()
            },
        };
        match sections.last_mut() {
            Some(s) if s.heading == heading => s.items.push(reading),
            _ => sections.push(ReadingSection {
                module: list_name.clone(),
                text: descriptions.remove(&heading).unwrap_or_default(),
                heading,
                source: "talis".into(),
                items: vec![reading],
                ..Default::default()
            }),
        }
    }
    Ok(sections)
}

/// Builds a reading from one Talis item's graph.
fn talis_item(g: &Graph, item_uri: &str, base: &str) -> Reading {
    let first = |s: &str, p: &str| values(g, s, p).first().map(|v| v.trim().to_owned());
    let note = first(item_uri, "studentNote")
        .or_else(|| first(item_uri, "note"))
        .map(|n| crate::html_to_markdown(&n))
        .unwrap_or_default();
    let Some(resource) = first(item_uri, "resource") else {
        // A paragraph: the lecturer's own words between items.
        return Reading {
            title: note
                .lines()
                .next()
                .unwrap_or("note")
                .chars()
                .take(120)
                .collect(),
            kind: "note".into(),
            note,
            ..Default::default()
        };
    };
    let types: Vec<&str> = values(g, &resource, "type");
    let kind = if types.iter().any(|t| t.ends_with("/Book")) {
        "book"
    } else if types.iter().any(|t| t.ends_with("/Chapter")) {
        "chapter"
    } else if types
        .iter()
        .any(|t| t.ends_with("/Article") || t.ends_with("/AcademicArticle"))
    {
        "article"
    } else if types
        .iter()
        .any(|t| t.ends_with("/Journal") || t.ends_with("/Periodical"))
    {
        "journal"
    } else if types
        .iter()
        .any(|t| t.ends_with("/Webpage") || t.ends_with("/Website") || t.ends_with("/Document"))
    {
        "web"
    } else if types
        .iter()
        .any(|t| t.contains("Audio") || t.contains("Film") || t.contains("Video"))
    {
        "video"
    } else {
        "unknown"
    };
    let people = |res: &str| -> Vec<String> {
        let mut names = Vec::new();
        for list in values(g, res, "authorList")
            .into_iter()
            .chain(values(g, res, "editorList"))
        {
            for person in members(g, list) {
                if let Some(n) = first(&person, "name") {
                    names.push(n);
                }
            }
        }
        if names.is_empty() {
            for p in values(g, res, "creator")
                .into_iter()
                .chain(values(g, res, "contributor"))
            {
                if let Some(n) = first(p, "name") {
                    names.push(n);
                }
            }
        }
        names
    };
    let mut authors = people(&resource);
    let parent = first(&resource, "isPartOf");
    let mut details: Vec<String> = Vec::new();
    if let Some(p) = &parent {
        if let Some(t) = first(p, "title") {
            details.push(t);
        }
        if authors.is_empty() {
            authors = people(p);
        }
    }
    for pred in ["edition", "volume", "issue", "pages", "date"] {
        if let Some(v) = first(&resource, pred) {
            details.push(match pred {
                "volume" => format!("vol. {v}"),
                "issue" => format!("no. {v}"),
                _ => v,
            });
        }
    }
    for p in values(g, &resource, "publisher") {
        if let Some(n) = first(p, "name") {
            details.push(n);
        }
    }
    let isbn = first(&resource, "isbn13")
        .or_else(|| first(&resource, "isbn10"))
        .or_else(|| first(&resource, "eissn"))
        .or_else(|| first(&resource, "issn"))
        .or_else(|| {
            parent
                .as_deref()
                .and_then(|p| first(p, "isbn13").or_else(|| first(p, "issn")))
        });
    let importance = first(item_uri, "importance")
        .map(|i| {
            i.rsplit(['/', '#'])
                .next()
                .unwrap_or_default()
                .replace("implicit-", "")
        })
        .map(|i| importance_in(&i).map_or(i, str::to_owned))
        .filter(|i| !i.is_empty());
    let online = values(g, &resource, "onlineResource")
        .first()
        .map(|s| (*s).to_owned())
        .or_else(|| first(&resource, "seeAlso"));
    let item_page = url::Url::parse(item_uri)
        .ok()
        .map(|u| format!("{base}{}.html", u.path()));
    Reading {
        title: first(&resource, "title").unwrap_or_else(|| "untitled".into()),
        kind: kind.into(),
        authors,
        details: details.join(", "),
        importance,
        url: online.or(item_page),
        doi: first(&resource, "doi"),
        isbn,
        file_id: None,
        note,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(kind: &str, title: &str) -> ModuleItem {
        ModuleItem {
            id: 1,
            title: title.into(),
            kind: kind.into(),
            html_url: None,
            external_url: None,
            page_url: (kind == "Page").then(|| title.to_lowercase().replace(' ', "-")),
            content_id: Some(7),
        }
    }

    fn module(name: &str, items: Vec<ModuleItem>) -> Module {
        Module {
            id: 1,
            name: name.into(),
            position: 1,
            unlock_at: None,
            items: Some(items),
            items_error: None,
            items_url: None,
        }
    }

    #[test]
    fn picks_items_under_reading_headings_and_reading_titles() {
        let m = module(
            "Seminar materials",
            vec![
                item("Page", "Attendance rules"),
                item("SubHeader", "Pre-reading for seminar 1 on Brexit"),
                item("File", "Brexit timeline"),
                item("File", "Seminar 1 slides"),
                item("SubHeader", "Assessment"),
                item("Assignment", "Essay"),
                item("Page", "Lecture 2 - Additional Reading"),
                item("Page", "Online Quiz - Preparation"),
            ],
        );
        let picked = pick(&[m]);
        let titles: Vec<&str> = picked.iter().map(|p| p.item.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Brexit timeline",
                "Seminar 1 slides",
                "Lecture 2 - Additional Reading"
            ]
        );
        assert_eq!(picked[0].heading, "Pre-reading for seminar 1 on Brexit");
        assert_eq!(picked[2].heading, "Assessment");
    }

    #[test]
    fn a_reading_module_takes_everything() {
        let picked = pick(&[module(
            "Reading List",
            vec![item("ExternalUrl", "Smith 2020")],
        )]);
        assert_eq!(picked.len(), 1);
    }

    #[test]
    fn links_skip_images_and_keep_link_text() {
        let md = "See [Morrison, Global Business](https://libsearch.example.ac.uk/x?id=1) and \
                  [![logo](https://c/img.png)](https://www.youtube.com/watch?v=1) plus https://doi.org/10.1000/xyz123.";
        let links = links_in(md);
        assert_eq!(links.len(), 3, "{links:?}");
        assert_eq!(links[0].kind, "library");
        assert_eq!(links[1].kind, "video");
        assert_eq!(links[2].kind, "article");
        assert_eq!(links[2].doi.as_deref(), Some("10.1000/xyz123"));
        let more = links_in(
            "[**PwC**](https://www.pwc.co.uk/) [Week 1 slides](https://c.example/courses/9/files/42?wrap=1)",
        );
        assert_eq!(more[0].title, "PwC");
        assert_eq!(more[1].kind, "file");
        assert_eq!(more[1].file_id, Some(42));
    }

    #[test]
    fn importance_from_words() {
        assert_eq!(
            importance_in("Pre-lecture video (optional /recommended)"),
            Some("recommended")
        );
        assert_eq!(importance_in("Essential reading"), Some("essential"));
        assert_eq!(importance_in("Required: chapter 3"), Some("essential"));
        assert_eq!(
            importance_in("Lecture 2 - Additional Reading"),
            Some("further")
        );
        assert_eq!(importance_in("Optional"), Some("optional"));
        assert_eq!(importance_in("Week 5"), None);
        assert_eq!(importance_in("Textbook and Additional Readings"), None);
    }

    #[test]
    fn links_take_importance_from_line_or_heading() {
        let md = "## Essential reading\n\n[Smith 2020](https://a.example/1)\n\n\
                  ## Further reading\n\n[Jones](https://a.example/2)\n\n\
                  Optional: [Lee](https://a.example/3)";
        let links = links_in(md);
        let imp: Vec<_> = links.iter().map(|r| r.importance.as_deref()).collect();
        assert_eq!(imp, [Some("essential"), Some("further"), Some("optional")]);
    }

    #[test]
    fn bare_links_take_the_nearby_heading_as_title() {
        let md = "[](https://iptv.example/View.aspx?id=1)\n\n\
                  ### The Secret Genius of Modern Life\n\n\
                  Follow the link:\n\n\
                  [https://iptv.example/View.aspx?id=1](https://iptv.example/View.aspx?id=1)";
        let links = links_in(md);
        assert_eq!(links.len(), 1, "{links:?}");
        assert_eq!(links[0].title, "The Secret Genius of Modern Life");
    }

    #[test]
    fn finds_isbns_in_citations() {
        assert_eq!(
            isbns_in("Morrison (2023), ISBN: 978-1-350-31234-5."),
            ["9781350312345"]
        );
    }

    #[test]
    fn recognises_list_systems() {
        assert_eq!(
            list_system(
                "https://kent.rl.talis.com/lists/10F8B64F-6799-A360-9494-C1BD7AA9530F.html"
            ),
            Some("talis")
        );
        assert_eq!(
            list_system("https://ncl.leganto.exlibrisgroup.com/leganto/public/44NCL/lists/123"),
            Some("leganto")
        );
        assert_eq!(list_system("https://example.com/notes"), None);
        assert_eq!(
            talis_list_id("https://x.rl.talis.com/lists/10f8b64f-6799-a360-9494-c1bd7aa9530f.html")
                .as_deref(),
            Some("10F8B64F-6799-A360-9494-C1BD7AA9530F")
        );
    }

    #[test]
    fn talis_members_are_ordered_numerically() {
        let g: Graph = serde_json::from_str(&format!(
            r#"{{"s":{{"{RDF}_10":[{{"value":"j"}}],"{RDF}_2":[{{"value":"b"}}],"{RDF}_1":[{{"value":"a"}}]}}}}"#
        ))
        .unwrap();
        assert_eq!(members(&g, "s"), ["a", "b", "j"]);
    }

    #[test]
    fn talis_item_reads_title_authors_and_journal() {
        let g: Graph = serde_json::from_str(
            r#"{
              "https://u/items/I": {"http://purl.org/vocab/resourcelist/schema#resource":[{"value":"https://u/resources/R"}]},
              "https://u/resources/R": {
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type":[{"value":"http://purl.org/ontology/bibo/Article"}],
                "http://purl.org/dc/terms/title":[{"value":"Sunk Costs"}],
                "http://purl.org/ontology/bibo/authorList":[{"value":"https://u/resources/R/authors"}],
                "http://purl.org/dc/terms/isPartOf":[{"value":"https://u/resources/J"}],
                "http://purl.org/ontology/bibo/pages":[{"value":"317-333"}]},
              "https://u/resources/R/authors": {"http://www.w3.org/1999/02/22-rdf-syntax-ns#_1":[{"value":"https://u/people/P"}]},
              "https://u/people/P": {"http://xmlns.com/foaf/0.1/name":[{"value":"John Nachbar"}]},
              "https://u/resources/J": {"http://purl.org/dc/terms/title":[{"value":"Journal of industrial economics"}]}
            }"#,
        )
        .unwrap();
        let r = talis_item(&g, "https://u/items/I", "https://x.rl.talis.com");
        assert_eq!(r.title, "Sunk Costs");
        assert_eq!(r.kind, "article");
        assert_eq!(r.authors, ["John Nachbar"]);
        assert_eq!(r.details, "Journal of industrial economics, 317-333");
        assert_eq!(
            r.url.as_deref(),
            Some("https://x.rl.talis.com/items/I.html")
        );
    }

    #[tokio::test]
    async fn reads_a_course_end_to_end_and_reports_unread_lists() {
        use crate::client::tests::{client, ok_json, serve};
        let modules = r#"[{"id":1,"name":"Week 5","position":1,"items":[
            {"id":10,"title":"Week 5: Platforms","type":"SubHeader"},
            {"id":11,"title":"Pre-lecture reading","type":"Page","page_url":"pre-lecture-reading"},
            {"id":12,"title":"Week 5 quiz","type":"Assignment"},
            {"id":13,"title":"Reading list","type":"ExternalUrl",
             "external_url":"https://x.leganto.exlibrisgroup.com/leganto/public/X/lists/1"}]}]"#;
        let page = r#"{"url":"pre-lecture-reading","title":"Pre-lecture reading",
            "body":"<p>Read <a href=\"https://doi.org/10.1000/abc\">Smith (2020)</a>.</p>"}"#;
        let tabs = r#"[{"id":"context_external_tool_9","label":"Reading List","type":"external",
            "html_url":"/courses/5/external_tools/9","full_url":"https://canvas.example/courses/5/external_tools/9"}]"#;
        let (base, _seen) = serve(vec![
            ok_json(modules, None),
            ok_json(page, None),
            ok_json(tabs, None),
        ])
        .await;
        let r = client(&base).readings(5, Some("week 5")).await.unwrap();
        assert!(r.warnings.is_empty(), "{:?}", r.warnings);
        assert_eq!(r.sections.len(), 2, "{:?}", r.sections);
        let s = &r.sections[0];
        assert_eq!(
            (s.heading.as_str(), s.title.as_str()),
            ("Week 5: Platforms", "Pre-lecture reading")
        );
        assert_eq!(s.items[0].kind, "article");
        assert_eq!(s.items[0].doi.as_deref(), Some("10.1000/abc"));
        assert_eq!(r.lists.len(), 2, "{:?}", r.lists);
        assert_eq!(r.lists[0].system, "leganto");
        assert!(r.lists[0].status.contains("sign-in"));
        assert_eq!(r.lists[1].system, "course-tool");
        assert!(r.lists.iter().all(|l| !l.status.is_empty()));
    }
}
