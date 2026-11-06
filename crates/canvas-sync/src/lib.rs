//! Incremental download of Canvas course files to a local folder.
//!
//! Layout: `<root>/<course code>/<folder>/<file name>`. The folder is the
//! file's Canvas folder without the leading `course files/`; when that is
//! hidden from the user (common for students), it is the name of the first
//! module linking the file instead.
//!
//! - **Sources.** The course Files list when the user may see it, plus
//!   every `File` item linked from a module. Many universities hide the Files list
//!   from students, so modules are usually the only source.
//! - **Incremental.** A manifest at `<root>/.canvas-sync/<course id>.json`
//!   records each file's `updated_at` and size. A file is downloaded again
//!   only if either changed or the local copy is missing.
//! - **Atomic.** Each download is written to `.canvas-sync/tmp/<id>.part`,
//!   checked against the size Canvas reports, and renamed into place, so a
//!   crash or a short read never leaves a half file under the real name.
//! - **Contained.** Every name from Canvas is sanitised, and the final
//!   path is checked to be inside `root`, so a hostile name cannot write
//!   elsewhere.
//! - **Pages.** [`save_pages`] saves wiki pages as Markdown in the same
//!   layout; see the `pages` module.
//! - **Honest.** Files that could not be fetched are listed in
//!   [`SyncReport::failed`]; callers must not report success when it is
//!   non-empty.

use std::collections::{BTreeMap, HashMap};
use std::path::{Component, Path, PathBuf};

use canvas_api::{CanvasFile, Client, Course};
use canvas_core::{Error, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use canvas_core::sanitize;
pub use pages::{PageFailure, PagesReport, save_pages};

mod pages;

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
    /// Bytes downloaded this run.
    pub bytes: u64,
    /// Things that went wrong without failing a file, e.g. a fallback.
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

#[derive(Debug, Default, Serialize, Deserialize)]
struct Manifest {
    files: BTreeMap<u64, Entry>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Entry {
    path: String,
    size: u64,
    updated_at: Option<DateTime<Utc>>,
}

/// Lists a course's files from every source the user can see, without
/// downloading them.
///
/// # Errors
/// Fails if the module list cannot be read. A hidden Files list is not an
/// error. Individual files that cannot be read are returned as failures.
pub async fn list_files(
    client: &Client,
    course: &Course,
) -> Result<(Vec<CanvasFile>, Vec<Failure>)> {
    list_files_cached(client, course, &HashMap::new()).await
}

/// Like [`list_files`], but reuses `known` file records (by id) instead of
/// fetching each one again. Only files not in `known` are looked up, a few
/// at a time. The module list is always read, so new files are always
/// found; a file replaced under the same id is only noticed when the
/// caller passes an empty `known` (the digest does that daily).
///
/// # Errors
/// As for [`list_files`].
pub async fn list_files_cached(
    client: &Client,
    course: &Course,
    known: &HashMap<u64, CanvasFile>,
) -> Result<(Vec<CanvasFile>, Vec<Failure>)> {
    let (files, _, failed) = collect(client, course, known).await?;
    Ok((files, failed))
}

/// File lookups in flight at once within one course.
const FILE_CONCURRENCY: usize = 6;

/// Lists files and remembers, for each, the first module that links it.
async fn collect(
    client: &Client,
    course: &Course,
    known: &HashMap<u64, CanvasFile>,
) -> Result<(Vec<CanvasFile>, HashMap<u64, String>, Vec<Failure>)> {
    let mut module_of: HashMap<u64, String> = HashMap::new();
    let mut files: HashMap<u64, CanvasFile> = HashMap::new();
    match client.files(course.id).await {
        Ok(list) => files.extend(list.into_iter().map(|f| (f.id, f))),
        Err(Error::Forbidden(_) | Error::NotFound(_)) => {}
        Err(e) => return Err(e),
    }
    let mut failed = Vec::new();
    let modules = match client.modules(course.id).await {
        Ok(m) => m,
        // A course can hide Modules as well as Files; then it has nothing
        // the user can list.
        Err(Error::Forbidden(_) | Error::NotFound(_)) => Vec::new(),
        Err(e) => return Err(e),
    };
    let mut wanted: Vec<(u64, String)> = Vec::new();
    for module in modules {
        for item in module.items.unwrap_or_default() {
            let Some(id) = item.content_id.filter(|_| item.kind == "File") else {
                continue;
            };
            module_of.entry(id).or_insert_with(|| module.name.clone());
            if files.contains_key(&id) || wanted.iter().any(|(w, _)| *w == id) {
                continue;
            }
            match known.get(&id) {
                Some(f) => {
                    files.insert(id, f.clone());
                }
                None => wanted.push((id, item.title)),
            }
        }
    }
    use futures::StreamExt;
    let fetched: Vec<(u64, String, Result<CanvasFile>)> = futures::stream::iter(wanted)
        .map(|(id, title)| async move { (id, title, client.file(course.id, id).await) })
        .buffer_unordered(FILE_CONCURRENCY)
        .collect()
        .await;
    for (id, title, result) in fetched {
        match result {
            Ok(f) => {
                files.insert(id, f);
            }
            Err(e @ Error::SessionExpired) => return Err(e),
            Err(e) => failed.push(Failure {
                file_id: id,
                name: title,
                reason: e.to_string(),
            }),
        }
    }
    failed.sort_by_key(|f| f.file_id);
    let mut files: Vec<CanvasFile> = files.into_values().collect();
    files.sort_by_key(|f| f.id);
    Ok((files, module_of, failed))
}

/// Downloads a course's new and changed files under `root`.
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
    progress: impl Fn(&str),
) -> Result<SyncReport> {
    let io = |path: &Path| {
        let path = path.to_owned();
        move |source| Error::Io { path, source }
    };
    std::fs::create_dir_all(root).map_err(io(root))?;
    let root = root.canonicalize().map_err(io(root))?;
    let (files, module_of, failed) = collect(client, course, &HashMap::new()).await?;
    let mut warnings = Vec::new();
    let folders: HashMap<u64, String> = match client.folders(course.id).await {
        Ok(list) => list.into_iter().map(|f| (f.id, f.full_name)).collect(),
        Err(Error::SessionExpired) => return Err(Error::SessionExpired),
        Err(e) => {
            warnings.push(format!(
                "folders unavailable ({e}); grouping files by module"
            ));
            HashMap::new()
        }
    };

    let sync_dir = root.join(".canvas-sync");
    let tmp_dir = sync_dir.join("tmp");
    std::fs::create_dir_all(&tmp_dir).map_err(io(&tmp_dir))?;
    let manifest_path = sync_dir.join(format!("{}.json", course.id));
    let mut manifest: Manifest = match std::fs::read_to_string(&manifest_path) {
        Ok(text) => serde_json::from_str(&text).unwrap_or_else(|e| {
            warnings.push(format!(
                "ignoring unreadable manifest ({e}); re-checking every file"
            ));
            Manifest::default()
        }),
        Err(_) => Manifest::default(),
    };

    let course_dir = sanitize(course.course_code.as_deref().unwrap_or(&course.name));
    let mut report = SyncReport {
        course_id: course.id,
        failed,
        warnings,
        ..Default::default()
    };
    // Keys are lowercased: macOS file systems ignore case by default, so
    // `Slides.pdf` and `slides.pdf` are the same file there.
    let mut taken: HashMap<String, u64> = HashMap::new();

    for file in files {
        let canvas_folder: Vec<&str> = file
            .folder_id
            .and_then(|id| folders.get(&id))
            .map_or("", String::as_str)
            .split('/')
            .skip_while(|p| *p == "course files")
            .filter(|p| !p.is_empty())
            .collect();
        let mut rel = PathBuf::from(&course_dir);
        if canvas_folder.is_empty() {
            // Root-level or hidden folder: group by the linking module.
            if let Some(module) = module_of.get(&file.id) {
                rel.push(sanitize(module));
            }
        } else {
            for part in canvas_folder {
                rel.push(sanitize(part));
            }
        }
        let key = |p: &Path| p.to_string_lossy().to_lowercase();
        let mut name = sanitize(&file.display_name);
        if taken
            .get(&key(&rel.join(&name)))
            .is_some_and(|id| *id != file.id)
        {
            name = with_id(&name, file.id);
        }
        rel.push(&name);
        if taken.get(&key(&rel)).is_some_and(|id| *id != file.id) {
            report.failed.push(Failure {
                file_id: file.id,
                name: file.display_name,
                reason: "another file already uses this name".into(),
            });
            continue;
        }
        taken.insert(key(&rel), file.id);
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
        };
        let up_to_date = manifest.files.get(&file.id) == Some(&entry)
            && std::fs::metadata(&dest).is_ok_and(|m| m.len() == file.size);
        if up_to_date {
            report.unchanged += 1;
            continue;
        }

        progress(&rel_text);
        // Temp files live under .canvas-sync/tmp, named by id, so they can
        // never collide with a real Canvas file; same file system, so the
        // rename stays atomic.
        let part = tmp_dir.join(format!("{}.part", file.id));
        let outcome = match client.download(&file.url, &part, Some(file.size)).await {
            Ok(bytes) => dest
                .parent()
                .map_or(Ok(()), std::fs::create_dir_all)
                .and_then(|()| std::fs::rename(&part, &dest))
                .map(|()| bytes)
                .map_err(|source| Error::Io {
                    path: dest.clone(),
                    source,
                }),
            Err(e) => Err(e),
        };
        match outcome {
            Ok(bytes) => {
                report.bytes += bytes;
                report.downloaded.push(rel_text);
                manifest.files.insert(file.id, entry);
            }
            Err(e) => {
                let _ = std::fs::remove_file(&part);
                if matches!(e, Error::SessionExpired) {
                    save(&manifest_path, &manifest)?;
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
    save(&manifest_path, &manifest)?;
    Ok(report)
}

fn save(path: &Path, manifest: &Manifest) -> Result<()> {
    let io = |source| Error::Io {
        path: path.to_owned(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let text = serde_json::to_string_pretty(manifest).map_err(|e| Error::Decode {
        what: "sync manifest".into(),
        detail: e.to_string(),
    })?;
    let tmp = path.with_extension("json.part");
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, path).map_err(io)
}

/// Inserts ` (<id>)` before the extension, to separate same-named files.
fn with_id(name: &str, id: u64) -> String {
    match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem} ({id}).{ext}"),
        _ => format!("{name} ({id})"),
    }
}

/// True if `path` stays under `root` using only normal components.
fn is_inside(root: &Path, path: &Path) -> bool {
    path.strip_prefix(root)
        .is_ok_and(|rel| rel.components().all(|c| matches!(c, Component::Normal(_))))
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
    }
}
