//! Incremental download of Canvas course files to a local folder.
//!
//! Layout: `<root>/<course code>/<folder>/<file name>`. The folder is the
//! file's Canvas folder without the leading `course files/`; when that is
//! hidden from the user (common for students), it is the name of the first
//! module linking the file instead.
//!
//! - **Sources.** The course Files list when the user may see it, every
//!   `File` item linked from a module, and, when asked (`linked`), every
//!   file of the course linked (`<a href>`) from the body of a module's
//!   pages. Many universities hide the Files list from students and link
//!   slides from pages, so pages can be the only source.
//! - **Placement.** A file found only through a page link goes under the
//!   module of the first page (in module order) that links it, unless its
//!   Canvas folder is visible. Files found any other way are placed
//!   exactly as before, so a file never changes folder between a sync with
//!   and without `linked`.
//! - **Stable names.** Two files that would share a name get it in turn:
//!   a file keeps the name the manifest already gives it; otherwise Files
//!   list and module files choose before page-only files, by id, and the
//!   later one takes ` (<id>)`. A plain sync keeps clear of the names of
//!   page-only files fetched earlier (it does not see them), so no run
//!   writes over another file's copy.
//! - **Incremental.** A manifest at `<root>/.canvas-sync/<course id>.json`
//!   records each file's path, `updated_at` and size, and whether it lies
//!   outside the Files list and modules. A file is downloaded again only if
//!   any changed or the local copy is missing. [`get_files`] records what it
//!   fetches there too, so a later sync knows it.
//! - **One writer at a time.** A sync or get holds the course's lock,
//!   `<course id>.lock`, from reading the manifest to writing it, so two
//!   of them (in one process or two) never pick the same name or drop each
//!   other's entries; the second waits. Temp names are unique.
//! - **Atomic.** Each download is written to a temp file under
//!   `.canvas-sync/tmp`, checked against the size Canvas reports, and
//!   renamed into place, so a crash or a short read never leaves a half
//!   file under the real name.
//! - **Contained.** Every name from Canvas is sanitised, and the final
//!   path is checked to be inside `root`, so a hostile name cannot write
//!   elsewhere.
//! - **Pages.** [`save_pages`] saves wiki pages as Markdown in the same
//!   layout; see the `pages` module.
//! - **Honest.** Files that could not be fetched are listed in `failed`.
//!   Page links to files Canvas hides or refuses are listed in
//!   `unavailable` (the student cannot fix those, so they are not
//!   failures), and pages that could not be read in `warnings`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use canvas_api::{CanvasFile, Client, Course, Module};
use canvas_core::{Error, Result};
use chrono::{DateTime, Utc};
use futures::StreamExt;
use serde::{Deserialize, Serialize};

pub use canvas_core::sanitize;
pub use pages::{PageFailure, PagesReport, save_pages};

mod pages;

/// File lookups in flight at once within one course.
const FILE_CONCURRENCY: usize = 6;

/// Page fetches in flight at once when looking for linked files.
const PAGE_CONCURRENCY: usize = 6;

/// Makes temp file names unique within this process.
static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// What one sync did.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct SyncReport {
    /// Course id.
    pub course_id: u64,
    /// Files downloaded this run, as paths relative to the root.
    pub downloaded: Vec<String>,
    /// Files already up to date.
    pub unchanged: usize,
    /// Files Canvas lists but locks for this user.
    pub locked: Vec<String>,
    /// Files that failed, with the reason.
    pub failed: Vec<Failure>,
    /// Files linked from pages that Canvas hides or refuses (a stale or
    /// unpublished link). Not failures: the student cannot fix them.
    pub unavailable: Vec<Failure>,
    /// Bytes downloaded this run.
    pub bytes: u64,
    /// Things that went wrong without failing a file, e.g. a fallback, or
    /// a page that could not be read for links.
    pub warnings: Vec<String>,
}

/// A file that could not be synced.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Failure {
    /// Canvas file id.
    pub file_id: u64,
    /// File name, if known.
    pub name: String,
    /// Why it failed.
    pub reason: String,
}

/// Where a listed file was found.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// The course Files list.
    Files,
    /// A module `File` item.
    Module,
    /// A link in the body of a module's page.
    Page,
}

/// A course file as listed, with where it was found.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ListedFile {
    /// The file's metadata.
    #[serde(flatten)]
    pub file: CanvasFile,
    /// Where it was found. A file found several ways takes the first of
    /// Files list, module item, page link.
    pub source: Source,
    /// For `source: page`, the url slug of the first page linking it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<String>,
}

/// A course's files from every source the user can see.
#[derive(Debug, Default, Clone, PartialEq, Serialize)]
pub struct FileList {
    /// The files, by id.
    pub files: Vec<ListedFile>,
    /// Files that could not be read.
    pub failed: Vec<Failure>,
    /// Files linked from pages that Canvas hides or refuses.
    pub unavailable: Vec<Failure>,
    /// Pages that could not be read for links.
    pub warnings: Vec<String>,
}

/// A file saved by [`get_files`].
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct SavedFile {
    /// Where it is on disk.
    pub path: PathBuf,
    /// Canvas file id.
    pub id: u64,
    /// Name shown in Canvas.
    pub name: String,
    /// Size in bytes.
    pub size: u64,
    /// Last change in Canvas.
    pub updated_at: Option<DateTime<Utc>>,
    /// False when the local copy was already up to date.
    pub downloaded: bool,
    /// Things that went wrong without failing, e.g. a page that could not
    /// be read while placing the file.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    files: BTreeMap<u64, Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    path: String,
    size: u64,
    updated_at: Option<DateTime<Utc>>,
    /// True for a file outside the Files list and module items: found
    /// through a page link, or fetched by a get. A plain sync does not see
    /// these files, so it keeps clear of their paths.
    #[serde(default, skip_serializing_if = "is_false")]
    linked: bool,
}

/// For `skip_serializing_if`: leaves a `false` flag out of the manifest.
#[allow(clippy::trivially_copy_pass_by_ref)]
fn is_false(flag: &bool) -> bool {
    !*flag
}

/// Lists a course's files from every source the user can see, including
/// files linked from module pages, without downloading them.
///
/// # Errors
/// Fails if the module list cannot be read. A hidden Files list is not an
/// error. Files that cannot be read are returned in `failed`, page links
/// Canvas hides or refuses in `unavailable`, and pages that cannot be read
/// in `warnings`.
pub async fn list_files(client: &Client, course: &Course) -> Result<FileList> {
    let found = collect(client, course, &HashMap::new(), true).await?;
    let files = found
        .files
        .into_iter()
        .map(|file| {
            let (source, page) = found
                .source
                .get(&file.id)
                .cloned()
                .unwrap_or((Source::Files, None));
            ListedFile { file, source, page }
        })
        .collect();
    Ok(FileList {
        files,
        failed: found.failed,
        unavailable: found.unavailable,
        warnings: found.warnings,
    })
}

/// Lists a course's files from the Files list and module items (not page
/// links, which would mean reading every page), reusing `known` file
/// records (by id) instead of fetching each one again. Only files not in
/// `known` are looked up, a few at a time. The module list is always read,
/// so new files are always found; a file replaced under the same id is
/// only noticed when the caller passes an empty `known` (the digest does
/// that daily).
///
/// # Errors
/// As for [`list_files`].
pub async fn list_files_cached(
    client: &Client,
    course: &Course,
    known: &HashMap<u64, CanvasFile>,
) -> Result<(Vec<CanvasFile>, Vec<Failure>)> {
    let found = collect(client, course, known, false).await?;
    Ok((found.files, found.failed))
}

/// Everything [`collect`] learns about a course's files.
#[derive(Default)]
struct Found {
    /// The files, by id.
    files: Vec<CanvasFile>,
    /// How each file was found, and for page links the page's slug.
    source: HashMap<u64, (Source, Option<String>)>,
    /// The module that places a file whose Canvas folder is hidden.
    module_of: HashMap<u64, String>,
    failed: Vec<Failure>,
    unavailable: Vec<Failure>,
    warnings: Vec<String>,
}

/// A file to look up: id, a name for reports, and whether only a page
/// links it.
type Wanted = (u64, String, bool);

/// Lists files and remembers how each was found and which module places
/// it. With `linked`, also reads every module page for file links.
async fn collect(
    client: &Client,
    course: &Course,
    known: &HashMap<u64, CanvasFile>,
    linked: bool,
) -> Result<Found> {
    let mut found = Found::default();
    let mut files: HashMap<u64, CanvasFile> = HashMap::new();
    match client.files(course.id).await {
        Ok(list) => {
            for f in list {
                found.source.insert(f.id, (Source::Files, None));
                files.insert(f.id, f);
            }
        }
        Err(Error::Forbidden(_) | Error::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    let modules = modules_of(client, course).await?;
    let mut wanted: Vec<Wanted> = Vec::new();
    let mut want = |w: Wanted, files: &mut HashMap<u64, CanvasFile>| match known.get(&w.0) {
        Some(f) => {
            files.insert(w.0, f.clone());
        }
        None => wanted.push(w),
    };
    for module in &modules {
        for item in module.items.iter().flatten() {
            let Some(id) = item.content_id.filter(|_| item.kind == "File") else {
                continue;
            };
            found
                .module_of
                .entry(id)
                .or_insert_with(|| module.name.clone());
            if found.source.contains_key(&id) {
                continue;
            }
            found.source.insert(id, (Source::Module, None));
            want((id, item.title.clone(), false), &mut files);
        }
    }
    if linked {
        let pages = module_pages(&modules);
        let links = page_links(client, course.id, &pages, None, &mut found.warnings).await?;
        for (id, slug, module) in links {
            // Found another way: placed and labelled as before.
            if found.source.contains_key(&id) {
                continue;
            }
            found.module_of.insert(id, module);
            want(
                (id, format!("file {id} linked from page {slug}"), true),
                &mut files,
            );
            found.source.insert(id, (Source::Page, Some(slug)));
        }
    }
    let fetched: Vec<(Wanted, Result<CanvasFile>)> = futures::stream::iter(wanted)
        .map(|w| async move {
            let file = client.file(course.id, w.0).await;
            (w, file)
        })
        .buffer_unordered(FILE_CONCURRENCY)
        .collect()
        .await;
    for ((id, name, from_page), result) in fetched {
        match result {
            Ok(f) if f.id != id => {
                // Canvas answers a replaced file's old id with its
                // replacement: file the record under its own id, once,
                // found and placed the way the old id was.
                let new = f.id;
                if let Some(how) = found.source.remove(&id) {
                    found.source.entry(new).or_insert(how);
                }
                if let Some(module) = found.module_of.get(&id).cloned() {
                    found.module_of.entry(new).or_insert(module);
                }
                files.entry(new).or_insert(f);
            }
            Ok(f) => {
                files.insert(id, f);
            }
            Err(e @ Error::SessionExpired) => return Err(e),
            Err(e) => {
                let gone = matches!(e, Error::NotFound(_) | Error::Forbidden(_));
                let failure = Failure {
                    file_id: id,
                    name,
                    reason: e.to_string(),
                };
                if from_page && gone {
                    found.unavailable.push(failure);
                } else {
                    found.failed.push(failure);
                }
            }
        }
    }
    found.failed.sort_by_key(|f| f.file_id);
    found.unavailable.sort_by_key(|f| f.file_id);
    found.files = files.into_values().collect();
    found.files.sort_by_key(|f| f.id);
    Ok(found)
}

/// A course's modules in order, or none if the course hides them.
async fn modules_of(client: &Client, course: &Course) -> Result<Vec<Module>> {
    match client.modules(course.id).await {
        Ok(m) => Ok(m),
        // A course can hide Modules as well as Files; then it has nothing
        // the user can list.
        Err(Error::Forbidden(_) | Error::NotFound(_)) => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

/// Each `Page` item's slug with its module's name, in module order, each
/// page once.
fn module_pages(modules: &[Module]) -> Vec<(String, String)> {
    let mut seen = HashSet::new();
    modules
        .iter()
        .flat_map(|m| {
            m.items
                .iter()
                .flatten()
                .filter(|i| i.kind == "Page")
                .filter_map(|i| i.page_url.clone())
                .map(|slug| (slug, m.name.clone()))
        })
        .filter(|(slug, _)| seen.insert(slug.clone()))
        .collect()
}

/// Reads `pages` in order and returns each file of the course they link,
/// once, with the first page linking it and that page's module. Stops
/// after the page that links `until`, if given. Pages that cannot be read
/// are noted in `warnings`.
async fn page_links(
    client: &Client,
    course_id: u64,
    pages: &[(String, String)],
    until: Option<u64>,
    warnings: &mut Vec<String>,
) -> Result<Vec<(u64, String, String)>> {
    // Owned strings: a stream of futures borrowing them is not `Send` for
    // every lifetime, which the MCP server's futures must be.
    let mut stream = futures::stream::iter(pages.to_vec())
        .map(|(slug, module)| async move {
            let links = client.page_file_links(course_id, &slug).await;
            (slug, module, links)
        })
        .buffered(PAGE_CONCURRENCY);
    let mut out: Vec<(u64, String, String)> = Vec::new();
    let mut seen = HashSet::new();
    while let Some((slug, module, result)) = stream.next().await {
        match result {
            Ok(ids) => {
                for id in ids {
                    if seen.insert(id) {
                        out.push((id, slug.clone(), module.clone()));
                    }
                }
                if until.is_some_and(|id| seen.contains(&id)) {
                    break;
                }
            }
            Err(e @ Error::SessionExpired) => return Err(e),
            Err(e) => warnings.push(format!("page `{slug}` not read for linked files: {e}")),
        }
    }
    Ok(out)
}

/// Where a file goes under `root`, before its name: the course folder,
/// then the file's Canvas folder without the leading `course files`, or,
/// when that is hidden or the top level, `module` if there is one.
fn folder_for(
    course_dir: &str,
    file: &CanvasFile,
    folders: &HashMap<u64, String>,
    module: Option<&str>,
) -> PathBuf {
    let canvas_folder: Vec<&str> = file
        .folder_id
        .and_then(|id| folders.get(&id))
        .map_or("", String::as_str)
        .split('/')
        .skip_while(|p| *p == "course files")
        .filter(|p| !p.is_empty())
        .collect();
    let mut rel = PathBuf::from(course_dir);
    if canvas_folder.is_empty() {
        // Root-level or hidden folder: group by the module.
        if let Some(module) = module {
            rel.push(sanitize(module));
        }
    } else {
        for part in canvas_folder {
            rel.push(sanitize(part));
        }
    }
    rel
}

/// The course's folders by id, or none (with a warning) if Canvas hides
/// them.
async fn folders_of(
    client: &Client,
    course: &Course,
    warnings: &mut Vec<String>,
) -> Result<HashMap<u64, String>> {
    match client.folders(course.id).await {
        Ok(list) => Ok(list.into_iter().map(|f| (f.id, f.full_name)).collect()),
        Err(Error::SessionExpired) => Err(Error::SessionExpired),
        Err(e) => {
            warnings.push(format!(
                "folders unavailable ({e}); grouping files by module"
            ));
            Ok(HashMap::new())
        }
    }
}

/// The course directory's name: its code, or its name if it has none.
fn course_dir(course: &Course) -> String {
    sanitize(course.course_code.as_deref().unwrap_or(&course.name))
}

/// Lowercased, for comparing paths: macOS file systems ignore case by
/// default, so `Slides.pdf` and `slides.pdf` are the same file there.
fn path_key(p: &Path) -> String {
    p.to_string_lossy().to_lowercase()
}

/// A temp file name no other writer (thread or process) will pick.
fn temp_name(stem: &str) -> String {
    format!(
        "{stem}.{}.{}.part",
        std::process::id(),
        TEMP_SEQ.fetch_add(1, Ordering::Relaxed)
    )
}

/// A file's two possible paths in `folder`: its own name, and its name
/// with ` (<id>)` for when another file has that.
fn names(folder: &Path, name: &str, id: u64) -> [PathBuf; 2] {
    [folder.join(name), folder.join(with_id(name, id))]
}

/// Lets file `id` keep the path the manifest gave it, if that is still one
/// of its `names` and no other file holds it.
fn keep_previous(
    held: &mut HashMap<String, u64>,
    manifest: &Manifest,
    names: &[PathBuf; 2],
    id: u64,
) {
    let Some(previous) = manifest.files.get(&id) else {
        return;
    };
    let key = previous.path.to_lowercase();
    if names.iter().any(|n| path_key(n) == key) && !held.contains_key(&key) {
        held.insert(key, id);
    }
}

/// True if file `id` already holds one of its `names`.
fn holds_one(held: &HashMap<String, u64>, names: &[PathBuf; 2], id: u64) -> bool {
    names.iter().any(|n| held.get(&path_key(n)) == Some(&id))
}

/// Picks file `id`'s path among its `names`: one it already holds, else
/// the first nobody holds, which it then holds. `None` if other files
/// hold both.
fn claim(held: &mut HashMap<String, u64>, names: &[PathBuf; 2], id: u64) -> Option<PathBuf> {
    if let Some(mine) = names.iter().find(|n| held.get(&path_key(n)) == Some(&id)) {
        return Some(mine.clone());
    }
    let free = names
        .iter()
        .find(|n| !held.contains_key(&path_key(n)))?
        .clone();
    held.insert(path_key(&free), id);
    Some(free)
}

/// A course's manifest on disk, shared by syncs and gets that may run at
/// the same time, in this process or another.
struct Store {
    course_id: u64,
    dir: PathBuf,
    path: PathBuf,
    lock: PathBuf,
    /// Temp files for this course's downloads. Only the lock holder
    /// downloads for the course, so it may empty this.
    tmp: PathBuf,
}

/// How long a get waits for another sync or get of the same course when
/// called for an agent: under the usual 60-second MCP request timeout.
pub const GET_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(45);

/// How long a sync (or a get run from a terminal) waits for another sync
/// or get of the same course.
pub const SYNC_LOCK_WAIT: std::time::Duration = std::time::Duration::from_secs(15 * 60);

/// Temp files older than this at the top of `.canvas-sync/tmp` (left by
/// earlier versions, which put them there) are removed.
const STALE_TEMP: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

impl Store {
    fn new(sync_dir: &Path, course_id: u64) -> Self {
        Self {
            course_id,
            dir: sync_dir.to_owned(),
            path: sync_dir.join(format!("{course_id}.json")),
            lock: sync_dir.join(format!("{course_id}.lock")),
            tmp: sync_dir.join("tmp").join(course_id.to_string()),
        }
    }

    /// Reads the manifest. A missing one is empty. One that is not a
    /// manifest (bad JSON, or not text) is kept aside as
    /// `<id>.json.unreadable-<time>` and a new one started, with a warning.
    ///
    /// # Errors
    /// [`Error::Io`] if the file cannot be read (say a network drive
    /// failing): the manifest is left alone rather than every file being
    /// downloaded again.
    fn load(&self, warnings: &mut Vec<String>) -> Result<Manifest> {
        let bytes = match std::fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Manifest::default()),
            Err(source) => {
                return Err(Error::Io {
                    path: self.path.clone(),
                    source,
                });
            }
        };
        let problem = match std::str::from_utf8(&bytes) {
            Ok(text) => match serde_json::from_str(text) {
                Ok(manifest) => return Ok(manifest),
                Err(e) => e.to_string(),
            },
            Err(e) => e.to_string(),
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        let aside = self.path.with_extension(format!("json.unreadable-{stamp}"));
        let kept = match std::fs::rename(&self.path, &aside) {
            Ok(()) => format!(", kept as {}", aside.display()),
            Err(_) => String::new(),
        };
        warnings.push(format!(
            "ignoring unreadable manifest ({problem}){kept}; re-checking every file"
        ));
        Ok(Manifest::default())
    }

    /// Takes the course's lock, waiting (without blocking the runtime) up
    /// to `limit` while another sync or get, in any process, holds it, and
    /// clears temp files an interrupted run left. The lock is released when
    /// the returned file is dropped.
    ///
    /// Where the file system cannot lock (some network drives), it carries
    /// on unguarded with a warning, as before locking existed.
    async fn lock(
        &self,
        limit: std::time::Duration,
        warnings: &mut Vec<String>,
    ) -> Result<Option<std::fs::File>> {
        let io = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::Io { path, source }
        };
        std::fs::create_dir_all(&self.tmp).map_err(io(&self.tmp))?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&self.lock)
            .map_err(io(&self.lock))?;
        let start = std::time::Instant::now();
        let held = loop {
            match file.try_lock() {
                Ok(()) => break Some(file),
                Err(std::fs::TryLockError::WouldBlock) if start.elapsed() < limit => {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(Error::Input(format!(
                        "another sync or get of course {} has been running for over {}s; \
                         try again when it finishes",
                        self.course_id,
                        limit.as_secs()
                    )));
                }
                Err(std::fs::TryLockError::Error(e))
                    if e.kind() == std::io::ErrorKind::Interrupted => {}
                Err(std::fs::TryLockError::Error(e)) => {
                    warnings.push(format!(
                        "this folder cannot lock files ({e}), so a sync or get of the same \
                         course running at the same time is not guarded against"
                    ));
                    break None;
                }
            }
        };
        let waited = start.elapsed().as_secs();
        if waited >= 1 {
            warnings.push(format!(
                "waited {waited}s for another sync or get of this course to finish"
            ));
        }
        if held.is_some() {
            // Only the lock holder downloads for this course, so anything
            // in its temp folder was left by an interrupted run.
            if let Ok(stale) = std::fs::read_dir(&self.tmp) {
                for entry in stale.flatten() {
                    let _ = std::fs::remove_file(entry.path());
                }
            }
            // Earlier versions kept temp files at the top; a day-old one is
            // no longer being written.
            if let Some(Ok(top)) = self.tmp.parent().map(std::fs::read_dir) {
                for entry in top.flatten() {
                    let old = entry.metadata().is_ok_and(|m| {
                        m.is_file()
                            && m.modified()
                                .is_ok_and(|t| t.elapsed().is_ok_and(|age| age > STALE_TEMP))
                    });
                    if old {
                        let _ = std::fs::remove_file(entry.path());
                    }
                }
            }
        }
        Ok(held)
    }

    /// Writes `changes` into the manifest as it is on disk now, each
    /// replacing any other entry for the same path. The caller holds the
    /// lock.
    fn merge(&self, changes: &BTreeMap<u64, Entry>, warnings: &mut Vec<String>) -> Result<()> {
        if changes.is_empty() {
            return Ok(());
        }
        let io = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::Io { path, source }
        };
        std::fs::create_dir_all(&self.dir).map_err(io(&self.dir))?;
        let mut manifest = self.load(warnings)?;
        for (id, entry) in changes {
            record(&mut manifest, *id, entry.clone());
        }
        let text = serde_json::to_string_pretty(&manifest).map_err(|e| Error::Decode {
            what: "sync manifest".into(),
            detail: e.to_string(),
        })?;
        let tmp = self.dir.join(temp_name("manifest"));
        std::fs::write(&tmp, text).map_err(io(&tmp))?;
        std::fs::rename(&tmp, &self.path).map_err(|source| {
            let _ = std::fs::remove_file(&tmp);
            Error::Io {
                path: self.path.clone(),
                source,
            }
        })
    }
}

/// Records file `id` at `entry`'s path, dropping any other entry for the
/// same path: one path holds one file, so the other entry is out of date.
fn record(manifest: &mut Manifest, id: u64, entry: Entry) {
    let key = entry.path.to_lowercase();
    manifest
        .files
        .retain(|other, e| *other == id || e.path.to_lowercase() != key);
    manifest.files.insert(id, entry);
}

/// Releases each name in `held` that an outside file (one in `outside`)
/// holds and one of `wanted` asks for, if Canvas no longer has that file:
/// it was deleted, or replaced (Canvas then answers its old id with the
/// replacement), so its old name is free. A file Canvas still has, or
/// cannot say about, keeps its name. Asks only on a clash, so a course
/// with none costs nothing.
async fn free_names_of_gone_files(
    client: &Client,
    course_id: u64,
    held: &mut HashMap<String, u64>,
    outside: &HashSet<u64>,
    wanted: impl Iterator<Item = &PathBuf>,
) -> Result<()> {
    let clashing: HashSet<u64> = wanted
        .filter_map(|n| held.get(&path_key(n)).copied())
        .filter(|id| outside.contains(id))
        .collect();
    for id in clashing {
        match client.file(course_id, id).await {
            Err(Error::NotFound(_)) => held.retain(|_, holder| *holder != id),
            Ok(f) if f.id != id => held.retain(|_, holder| *holder != id),
            Err(e @ Error::SessionExpired) => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// Downloads a course's new and changed files under `root`. With
/// `linked`, files linked from module pages are included.
///
/// `progress` is called with each file's relative path before download.
///
/// # Errors
/// Fails only if the course cannot be listed or the manifest cannot be
/// written. Per-file problems are reported in [`SyncReport::failed`];
/// an expired session aborts the sync with [`Error::SessionExpired`].
pub async fn sync_course(
    client: &Client,
    course: &Course,
    root: &Path,
    linked: bool,
    progress: impl Fn(&str),
) -> Result<SyncReport> {
    let io = |path: &Path| {
        let path = path.to_owned();
        move |source| Error::Io { path, source }
    };
    std::fs::create_dir_all(root).map_err(io(root))?;
    let root = root.canonicalize().map_err(io(root))?;
    let found = collect(client, course, &HashMap::new(), linked).await?;
    let mut warnings = found.warnings;
    let folders = folders_of(client, course, &mut warnings).await?;

    let sync_dir = root.join(".canvas-sync");
    let store = Store::new(&sync_dir, course.id);
    // Held until the manifest is written.
    let _lock = store.lock(SYNC_LOCK_WAIT, &mut warnings).await?;
    let tmp_dir = store.tmp.clone();
    let manifest = store.load(&mut warnings)?;

    let course_dir = course_dir(course);
    let mut report = SyncReport {
        course_id: course.id,
        failed: found.failed,
        unavailable: found.unavailable,
        ..Default::default()
    };
    let source = found.source;
    let is_page = |id: u64| matches!(source.get(&id), Some((Source::Page, _)));
    let in_run: HashSet<u64> = found.files.iter().map(|f| f.id).collect();
    // Lowercased path → the file holding it. Every recorded file outside
    // this run whose copy exists keeps its path: page-only files a plain
    // sync does not see, files a get took from a link elsewhere, files a
    // lecturer took out of a module but kept. A file of this run takes
    // such a path only if Canvas no longer has its holder.
    let mut held: HashMap<String, u64> = HashMap::new();
    let mut outside = HashSet::new();
    for (id, e) in &manifest.files {
        if !in_run.contains(id) && root.join(&e.path).is_file() {
            held.insert(e.path.to_lowercase(), *id);
            outside.insert(*id);
        }
    }
    // Files list and module files choose first, so a page-only file is the
    // one that gives way on a clash.
    let mut files = found.files;
    files.sort_by_key(|f| (is_page(f.id), f.id));
    let planned: Vec<(CanvasFile, [PathBuf; 2])> = files
        .into_iter()
        .map(|file| {
            let module = found.module_of.get(&file.id).map(String::as_str);
            let folder = folder_for(&course_dir, &file, &folders, module);
            let names = names(&folder, &sanitize(&file.display_name), file.id);
            (file, names)
        })
        .collect();
    for (file, names) in &planned {
        keep_previous(&mut held, &manifest, names, file.id);
    }
    // Files that could not keep their name may find it free: ask Canvas
    // about the outside files holding the names they want.
    let unsettled: Vec<PathBuf> = planned
        .iter()
        .filter(|(file, names)| !holds_one(&held, names, file.id))
        .flat_map(|(_, names)| names.iter().cloned())
        .collect();
    free_names_of_gone_files(client, course.id, &mut held, &outside, unsettled.iter()).await?;
    for (file, names) in &planned {
        keep_previous(&mut held, &manifest, names, file.id);
    }

    let mut changes: BTreeMap<u64, Entry> = BTreeMap::new();
    for (file, names) in planned {
        let Some(rel) = claim(&mut held, &names, file.id) else {
            report.failed.push(Failure {
                file_id: file.id,
                name: file.display_name,
                reason: "another file already uses this name".into(),
            });
            continue;
        };
        let rel_text = rel.to_string_lossy().into_owned();
        if file.locked_for_user || file.url.is_empty() {
            report.locked.push(rel_text);
            continue;
        }
        let dest = root.join(&rel);
        if !is_inside(&root, &dest) {
            report.failed.push(Failure {
                file_id: file.id,
                name: file.display_name,
                reason: "unsafe path".into(),
            });
            continue;
        }
        let entry = Entry {
            path: rel_text.clone(),
            size: file.size,
            updated_at: file.updated_at,
            linked: is_page(file.id),
        };
        let previous = manifest.files.get(&file.id);
        if is_current(previous, &entry, &dest) {
            report.unchanged += 1;
            if previous.is_some_and(|p| p.linked != entry.linked) {
                changes.insert(file.id, entry);
            }
            continue;
        }

        progress(&rel_text);
        match fetch_into(client, &file, &tmp_dir, &dest).await {
            Ok(bytes) => {
                report.bytes += bytes;
                report.downloaded.push(rel_text);
                changes.insert(file.id, entry);
            }
            Err(e) => {
                if matches!(e, Error::SessionExpired) {
                    store.merge(&changes, &mut warnings)?;
                    return Err(e);
                }
                report.failed.push(Failure {
                    file_id: file.id,
                    name: file.display_name,
                    reason: e.to_string(),
                });
            }
        }
    }
    store.merge(&changes, &mut warnings)?;
    report.warnings = warnings;
    Ok(report)
}

/// What placing files needs to know about a course, read once for a batch
/// of gets.
struct CourseView {
    folders: HashMap<u64, String>,
    /// For each module `File` item, the first module listing it.
    item_module: HashMap<u64, String>,
    /// Files a plain sync finds: the Files list and module items.
    base: HashSet<u64>,
    /// Module pages, in order, with their module.
    pages: Vec<(String, String)>,
    /// For files linked from pages, the module of the first page linking
    /// them. Read when first needed.
    page_module: Option<HashMap<u64, String>>,
}

impl CourseView {
    async fn read(client: &Client, course: &Course, warnings: &mut Vec<String>) -> Result<Self> {
        let folders = folders_of(client, course, warnings).await?;
        let modules = modules_of(client, course).await?;
        let mut item_module = HashMap::new();
        for m in &modules {
            for item in m.items.iter().flatten() {
                if let Some(id) = item.content_id.filter(|_| item.kind == "File") {
                    item_module.entry(id).or_insert_with(|| m.name.clone());
                }
            }
        }
        let mut base: HashSet<u64> = item_module.keys().copied().collect();
        match client.files(course.id).await {
            Ok(list) => base.extend(list.iter().map(|f| f.id)),
            Err(Error::Forbidden(_) | Error::NotFound(_)) => {}
            Err(e @ Error::SessionExpired) => return Err(e),
            Err(e) => warnings.push(format!(
                "Files list unavailable ({e}); placing files as if it were hidden"
            )),
        }
        Ok(Self {
            folders,
            item_module,
            base,
            pages: module_pages(&modules),
            page_module: None,
        })
    }

    /// The module a sync files `file_id` under when its folder is hidden:
    /// the first module with a `File` item for it; none if the Files list
    /// has it (sync finds it there); otherwise the module of the first page
    /// linking it. Pages are read once, stopping at `until` if given.
    async fn module_for(
        &mut self,
        client: &Client,
        course_id: u64,
        file_id: u64,
        until: Option<u64>,
        warnings: &mut Vec<String>,
    ) -> Result<Option<String>> {
        if let Some(m) = self.item_module.get(&file_id) {
            return Ok(Some(m.clone()));
        }
        if self.base.contains(&file_id) {
            return Ok(None);
        }
        if self.page_module.is_none() {
            let links = page_links(client, course_id, &self.pages, until, warnings).await?;
            self.page_module = Some(links.into_iter().map(|(id, _, m)| (id, m)).collect());
        }
        Ok(self
            .page_module
            .as_ref()
            .and_then(|m| m.get(&file_id).cloned()))
    }
}

/// Downloads files of `course`, by id, each into the place a sync would
/// put it, and records them in the course's manifest so a later sync
/// treats them as known. A file already up to date is not downloaded
/// again. `wait` is how long to wait for another sync or get of the course
/// ([`GET_LOCK_WAIT`] for an agent, [`SYNC_LOCK_WAIT`] in a terminal).
///
/// This is how to fetch files linked only from inside pages, or from
/// anywhere else sync does not look. Each file is read and placed (which
/// may mean reading every page) before the course is locked, so the lock
/// covers only naming, downloading and recording. Returns one result per
/// id, in order, plus warnings for the whole batch.
///
/// # Errors
/// [`Error::SessionExpired`] before any file, a root or manifest that
/// cannot be used, or another sync or get of the course still running
/// after `wait`. Per file: [`Error::NotFound`] or [`Error::Forbidden`],
/// each naming the file, when Canvas hides, refuses or locks it. A
/// per-file [`Error::SessionExpired`] ends the batch: it is the last
/// result, after the files already saved.
pub async fn get_files(
    client: &Client,
    course: &Course,
    root: &Path,
    ids: &[u64],
    wait: std::time::Duration,
) -> Result<(Vec<Result<SavedFile>>, Vec<String>)> {
    let io = |path: &Path| {
        let path = path.to_owned();
        move |source| Error::Io { path, source }
    };
    std::fs::create_dir_all(root).map_err(io(root))?;
    let root = root.canonicalize().map_err(io(root))?;
    let mut warnings = Vec::new();
    let mut view = CourseView::read(client, course, &mut warnings).await?;
    let course_dir = course_dir(course);
    // One file: stop reading pages once it is found.
    let until = match ids {
        [one] => Some(*one),
        _ => None,
    };
    let mut prepared = Vec::new();
    for &id in ids {
        let one = prepare(
            client,
            course,
            &course_dir,
            &mut view,
            id,
            until,
            &mut warnings,
        )
        .await;
        // An expired session fails every later file; stop after it.
        let expired = matches!(one, Err(Error::SessionExpired));
        prepared.push(one);
        if expired {
            break;
        }
    }
    let store = Store::new(&root.join(".canvas-sync"), course.id);
    // Held until the last file is recorded.
    let _lock = store.lock(wait, &mut warnings).await?;
    let mut manifest = store.load(&mut warnings)?;
    let mut results = Vec::new();
    for one in prepared {
        let result = match one {
            Ok(p) => {
                save_one(
                    client,
                    &root,
                    &store,
                    &view,
                    &mut manifest,
                    p,
                    &mut warnings,
                )
                .await
            }
            Err(e) => Err(e),
        };
        let expired = matches!(result, Err(Error::SessionExpired));
        results.push(result);
        if expired {
            break;
        }
    }
    Ok((results, warnings))
}

/// Downloads one file for an agent, as [`get_files`] does with
/// [`GET_LOCK_WAIT`], with the batch's warnings in [`SavedFile::warnings`].
///
/// # Errors
/// As for [`get_files`], with the file's own error returned directly.
pub async fn get_file(
    client: &Client,
    course: &Course,
    root: &Path,
    file_id: u64,
) -> Result<SavedFile> {
    let (results, warnings) = get_files(client, course, root, &[file_id], GET_LOCK_WAIT).await?;
    let mut saved = results
        .into_iter()
        .next()
        .unwrap_or_else(|| Err(Error::Input(format!("file {file_id} was not fetched"))))?;
    saved.warnings.splice(0..0, warnings);
    Ok(saved)
}

/// A file read and placed, ready to save.
struct Prepared {
    file: CanvasFile,
    /// Its folder under the root, course folder first.
    folder: PathBuf,
    /// True if it lies outside the Files list and module items.
    linked: bool,
    /// `file <id> in <course>`, for messages.
    what: String,
}

/// Reads file `id` and works out where a sync would put it.
async fn prepare(
    client: &Client,
    course: &Course,
    course_dir: &str,
    view: &mut CourseView,
    id: u64,
    until: Option<u64>,
    warnings: &mut Vec<String>,
) -> Result<Prepared> {
    let what = format!("file {id} in {course_dir}");
    let file = match client.file(course.id, id).await {
        Ok(f) => f,
        Err(Error::NotFound(_)) => {
            return Err(Error::NotFound(format!(
                "{what} (it may be hidden from you, or in another course)"
            )));
        }
        Err(Error::Forbidden(_)) => {
            return Err(Error::Forbidden(format!(
                "{what}: Canvas does not let you open it"
            )));
        }
        Err(e) => return Err(e),
    };
    // Names and lock reasons are Canvas text: quoted, never bare.
    if file.locked_for_user || file.url.is_empty() {
        let why = file
            .lock_explanation
            .as_deref()
            .map(canvas_api::html_to_markdown)
            .filter(|w| !w.is_empty())
            .map(|w| format!("; Canvas says {w:?}"))
            .unwrap_or_default();
        return Err(Error::Forbidden(format!(
            "{:?} ({what}) is locked{why}",
            file.display_name
        )));
    }
    // Canvas answers a replaced file's old id with its replacement, which
    // is then saved under its own id; links may name either.
    let ids = [id, file.id];
    let mut folder = folder_for(course_dir, &file, &view.folders, None);
    if folder == Path::new(course_dir) {
        for each in ids {
            if let Some(module) = view
                .module_for(client, course.id, each, until, warnings)
                .await?
            {
                folder.push(sanitize(&module));
                break;
            }
        }
    }
    let linked = !ids.iter().any(|i| view.base.contains(i));
    Ok(Prepared {
        file,
        folder,
        linked,
        what,
    })
}

/// Names, downloads and records one prepared file. The caller holds the
/// course's lock.
async fn save_one(
    client: &Client,
    root: &Path,
    store: &Store,
    view: &CourseView,
    manifest: &mut Manifest,
    p: Prepared,
    warnings: &mut Vec<String>,
) -> Result<SavedFile> {
    let Prepared {
        file,
        folder,
        linked,
        what,
    } = p;
    let id = file.id;
    // Paths other files keep, as a sync would keep clear of them: every
    // recorded file whose copy exists. One Canvas no longer lists may give
    // way if Canvas no longer has it.
    let mut held: HashMap<String, u64> = HashMap::new();
    let mut outside = HashSet::new();
    for (other, e) in &manifest.files {
        if *other != id && root.join(&e.path).is_file() {
            held.insert(e.path.to_lowercase(), *other);
            if !view.base.contains(other) {
                outside.insert(*other);
            }
        }
    }
    let names = names(&folder, &sanitize(&file.display_name), id);
    keep_previous(&mut held, manifest, &names, id);
    if !holds_one(&held, &names, id) {
        free_names_of_gone_files(client, store.course_id, &mut held, &outside, names.iter())
            .await?;
        keep_previous(&mut held, manifest, &names, id);
    }
    let rel = claim(&mut held, &names, id).ok_or_else(|| {
        Error::Input(format!(
            "{what}: other files already use both of its names in {}",
            folder.display()
        ))
    })?;
    let dest = root.join(&rel);
    if !is_inside(root, &dest) {
        return Err(Error::Input(format!(
            "refusing to write {what} outside {}",
            root.display()
        )));
    }
    let entry = Entry {
        path: rel.to_string_lossy().into_owned(),
        size: file.size,
        updated_at: file.updated_at,
        linked,
    };
    let previous = manifest.files.get(&id);
    let downloaded = !is_current(previous, &entry, &dest);
    let relabel = previous.is_some_and(|p| p.linked != linked);
    if downloaded {
        fetch_into(client, &file, &store.tmp, &dest)
            .await
            .map_err(|e| match e {
                Error::Forbidden(_) => Error::Forbidden(format!(
                    "Canvas refused to download {:?} ({what})",
                    file.display_name
                )),
                Error::NotFound(_) => Error::NotFound(format!(
                    "Canvas could not find the download for {:?} ({what})",
                    file.display_name
                )),
                other => other,
            })?;
    }
    if downloaded || relabel {
        store.merge(&BTreeMap::from([(id, entry.clone())]), warnings)?;
        record(manifest, id, entry);
    }
    Ok(SavedFile {
        path: dest,
        id,
        name: file.display_name,
        size: file.size,
        updated_at: file.updated_at,
        downloaded,
        warnings: Vec::new(),
    })
}

/// True if the manifest's `previous` entry matches `entry` (same path,
/// size and change time) and the local copy at `dest` has that size.
fn is_current(previous: Option<&Entry>, entry: &Entry, dest: &Path) -> bool {
    previous.is_some_and(|p| {
        p.path == entry.path && p.size == entry.size && p.updated_at == entry.updated_at
    }) && std::fs::metadata(dest).is_ok_and(|m| m.len() == entry.size)
}

/// Downloads `file` to a unique temp file in `tmp_dir`, checks its size,
/// and renames it to `dest`. The temp file is removed on failure.
async fn fetch_into(
    client: &Client,
    file: &CanvasFile,
    tmp_dir: &Path,
    dest: &Path,
) -> Result<u64> {
    // Temp files live under .canvas-sync/tmp, named by id plus a unique
    // tail, so they never collide with a real Canvas file or with another
    // writer fetching the same file; same file system, so the rename
    // stays atomic.
    let part = tmp_dir.join(temp_name(&file.id.to_string()));
    let outcome = match client.download(&file.url, &part, Some(file.size)).await {
        Ok(bytes) => dest
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::rename(&part, dest))
            .map(|()| bytes)
            .map_err(|source| Error::Io {
                path: dest.to_owned(),
                source,
            }),
        Err(e) => Err(e),
    };
    if outcome.is_err() {
        let _ = std::fs::remove_file(&part);
    }
    outcome
}

/// Inserts ` (<id>)` before the extension, to separate same-named files.
fn with_id(name: &str, id: u64) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({id}).{ext}"),
        _ => format!("{name} ({id})"),
    }
}

/// True if `path` is below `root` (not `root` itself) using only normal
/// components.
fn is_inside(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root).is_ok_and(|rel| {
        rel.components().next().is_some()
            && rel.components().all(|c| matches!(c, Component::Normal(_)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_blocks_traversal_and_reserved_characters() {
        for bad in ["..", ".", "", "   ", "/", "\\..\\"] {
            let s = sanitize(bad);
            assert!(
                !s.is_empty() && s != "." && s != ".." && !s.contains(['/', '\\']),
                "{bad:?} -> {s:?}"
            );
        }
        assert_eq!(sanitize("a\u{0}b\nc"), "a_b_c");
        assert_eq!(sanitize(".hidden"), "hidden");
        assert_eq!(sanitize("notes. "), "notes");
    }

    #[test]
    fn sanitize_caps_length_on_a_char_boundary() {
        let long = "é".repeat(200);
        let s = sanitize(&long);
        assert!(s.len() <= 150);
        assert!(s.chars().all(|c| c == 'é'));
    }

    #[test]
    fn duplicate_names_get_the_id() {
        assert_eq!(with_id("Slides.pdf", 7), "Slides (7).pdf");
        assert_eq!(with_id("README", 7), "README (7)");
        assert_eq!(with_id(".env", 7), ".env (7)");
    }

    #[test]
    fn containment_rejects_escapes() {
        let root = Path::new("/r");
        assert!(is_inside(root, Path::new("/r/a/b.pdf")));
        assert!(!is_inside(root, Path::new("/r/../x")));
        assert!(!is_inside(root, Path::new("/elsewhere/x")));
        assert!(!is_inside(root, Path::new("/r/a/../../x")));
        assert!(
            !is_inside(root, Path::new("/r")),
            "the root itself is not a file"
        );
    }

    #[test]
    fn hostile_folder_and_module_names_stay_inside() {
        let file: CanvasFile =
            serde_json::from_str(r#"{"id":1,"display_name":"x","folder_id":2}"#).unwrap();
        let folders = HashMap::from([(2, "course files/../../etc".to_owned())]);
        let rel = folder_for("ABC", &file, &folders, None);
        assert!(
            is_inside(Path::new("/r"), &Path::new("/r").join(&rel)),
            "{rel:?}"
        );
        let rel = folder_for("ABC", &file, &HashMap::new(), Some("../.."));
        assert!(
            is_inside(Path::new("/r"), &Path::new("/r").join(&rel)),
            "{rel:?}"
        );
    }

    fn entry(path: &str) -> Entry {
        Entry {
            path: path.into(),
            size: 1,
            updated_at: None,
            linked: false,
        }
    }

    #[test]
    fn names_stay_with_their_files() {
        let folder = Path::new("ABC/Week 1");
        let five = names(folder, "Slides.pdf", 5);
        let nine = names(folder, "slides.pdf", 9);
        // Fresh: the first to choose gets the plain name.
        let mut held = HashMap::new();
        assert_eq!(
            claim(&mut held, &five, 5).unwrap(),
            folder.join("Slides.pdf")
        );
        assert_eq!(
            claim(&mut held, &nine, 9).unwrap(),
            folder.join("slides (9).pdf")
        );
        // Next run, 9 chooses first but keeps its own name, and 5 keeps
        // the plain one.
        let manifest = Manifest {
            files: BTreeMap::from([
                (5, entry("ABC/Week 1/Slides.pdf")),
                (9, entry("ABC/Week 1/slides (9).pdf")),
            ]),
        };
        let mut held = HashMap::new();
        keep_previous(&mut held, &manifest, &nine, 9);
        keep_previous(&mut held, &manifest, &five, 5);
        assert_eq!(
            claim(&mut held, &nine, 9).unwrap(),
            folder.join("slides (9).pdf")
        );
        assert_eq!(
            claim(&mut held, &five, 5).unwrap(),
            folder.join("Slides.pdf")
        );
        // A file whose two names are both held has none.
        let mut both = HashMap::from([
            (path_key(&folder.join("x.pdf")), 1),
            (path_key(&folder.join("x (2).pdf")), 3),
        ]);
        assert_eq!(claim(&mut both, &names(folder, "x.pdf", 2), 2), None);
    }

    #[test]
    fn manifest_merges_keep_other_writers_entries() {
        let dir = std::env::temp_dir().join(format!("canvas-merge-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(&dir, 1);
        let mut warnings = Vec::new();
        store
            .merge(&BTreeMap::from([(5, entry("a"))]), &mut warnings)
            .unwrap();
        // Another writer that loaded the manifest before 5 was added.
        store
            .merge(&BTreeMap::from([(6, entry("b"))]), &mut warnings)
            .unwrap();
        let ids: Vec<u64> = store
            .load(&mut warnings)
            .unwrap()
            .files
            .into_keys()
            .collect();
        assert_eq!(ids, vec![5, 6]);
        // A newer entry for a path replaces the older one.
        store
            .merge(&BTreeMap::from([(7, entry("A"))]), &mut warnings)
            .unwrap();
        let ids: Vec<u64> = store
            .load(&mut warnings)
            .unwrap()
            .files
            .into_keys()
            .collect();
        assert_eq!(ids, vec![6, 7], "one path holds one file");
        // Unreadable manifests (bad JSON, bad text) are kept aside, not lost.
        for bad in [&b"{not json"[..], &[0xff, 0xfe, 0x00][..]] {
            std::fs::write(dir.join("1.json"), bad).unwrap();
            assert!(store.load(&mut warnings).unwrap().files.is_empty());
            assert!(!dir.join("1.json").exists());
        }
        let kept = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains("unreadable"))
            .count();
        assert!(kept >= 1, "kept aside");
        assert!(
            warnings.iter().any(|w| w.contains("unreadable")),
            "{warnings:?}"
        );
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_busy_course_lock_gives_up_with_a_clear_error() {
        let dir = std::env::temp_dir().join(format!("canvas-lock-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = Store::new(&dir, 1);
        let mut warnings = Vec::new();
        let first = store.lock(GET_LOCK_WAIT, &mut warnings).await.unwrap();
        assert!(first.is_some());
        let limit = std::time::Duration::from_millis(300);
        let err = store.lock(limit, &mut warnings).await.unwrap_err();
        assert!(
            err.to_string().contains("still running")
                || err.to_string().contains("running for over"),
            "{err}"
        );
        drop(first);
        assert!(store.lock(limit, &mut warnings).await.unwrap().is_some());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn pages_are_read_in_module_order_once() {
        let modules: Vec<Module> = serde_json::from_str(
            r#"[{"id":1,"name":"Week 1","items":[
                    {"id":1,"type":"Page","page_url":"intro"},
                    {"id":2,"type":"File","content_id":5},
                    {"id":3,"type":"Page","page_url":"slides"}]},
                {"id":2,"name":"Week 2","items":[
                    {"id":4,"type":"Page","page_url":"intro"},
                    {"id":5,"type":"Page","page_url":"workshop"}]}]"#,
        )
        .unwrap();
        let pages = module_pages(&modules);
        let slugs: Vec<(&str, &str)> = pages
            .iter()
            .map(|(s, m)| (s.as_str(), m.as_str()))
            .collect();
        assert_eq!(
            slugs,
            vec![
                ("intro", "Week 1"),
                ("slides", "Week 1"),
                ("workshop", "Week 2")
            ]
        );
    }
}
