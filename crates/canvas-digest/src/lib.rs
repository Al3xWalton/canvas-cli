//! What is new and due on Canvas since the last run.
//!
//! Notification agents should not diff Canvas themselves. [`digest`] makes
//! one pass over every active course and returns:
//!
//! - **changes since `since`**: new announcements, new or changed
//!   assignments, new or updated files, and new ReCap recordings;
//! - **upcoming**: unsubmitted assignments due in the next `upcoming` window
//!   (always included, since a reminder is still useful the next day).
//!
//! "Since the last run" is a [`State`] file, `<root>/.canvas-sync/last-digest`,
//! read by [`load_state`] and written by [`save_state`] only after a digest
//! succeeds. Besides the time, it remembers what was seen:
//!
//! - **recording ids**, so a lecture published days after it was recorded
//!   is still reported once (its start time is before the last run);
//! - **each assignment's due date and points**, so only real changes count,
//!   not Canvas bumping `updated_at` for unrelated edits;
//! - **each file's size**, so a re-saved but identical file is not news.
//!
//! On the first run (no state) those fall back to timestamps. Announcements
//! are always by `posted_at`, which is their publication time.
//!
//! [`build`] is the pure part: it takes everything fetched and the previous
//! state, so the logic is tested without a network.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use canvas_api::{Announcement, Assignment, CanvasFile, Client, Course};
use canvas_core::{Error, Result};
use chrono::{DateTime, Duration, Utc};
use futures::StreamExt;
use panopto::{Panopto, RecordingSummary};
use serde::{Deserialize, Serialize};

/// Courses fetched at once.
const CONCURRENCY: usize = 4;

/// What [`digest`] should look at.
#[derive(Debug, Clone)]
pub struct Options {
    /// Report changes after this time.
    pub since: DateTime<Utc>,
    /// Also list unsubmitted work due within this window.
    pub upcoming: Duration,
    /// Check course files (slower: one request per module file).
    pub files: bool,
    /// Check ReCap recordings (needs a ReCap session).
    pub recordings: bool,
}

/// An item with the course it belongs to.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InCourse<T> {
    /// Course id.
    pub course_id: u64,
    /// Course code or name.
    pub course: String,
    /// The item.
    #[serde(flatten)]
    pub item: T,
}

/// The digest.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Digest {
    /// Start of the reporting window.
    pub since: Option<DateTime<Utc>>,
    /// When the digest was made; pass to [`mark_run`].
    pub generated_at: Option<DateTime<Utc>>,
    /// Announcements posted since.
    pub new_announcements: Vec<Announcement>,
    /// Assignments created since.
    pub new_assignments: Vec<InCourse<Assignment>>,
    /// Assignments changed (not created) since, e.g. a moved deadline.
    pub changed_assignments: Vec<InCourse<Assignment>>,
    /// Files added or updated since.
    pub new_files: Vec<InCourse<CanvasFile>>,
    /// Recordings made since.
    pub new_recordings: Vec<InCourse<RecordingSummary>>,
    /// Unsubmitted assignments due in the upcoming window, soonest first.
    pub upcoming: Vec<InCourse<Assignment>>,
    /// Parts that could not be checked (e.g. ReCap signed out); the digest
    /// is incomplete for those.
    pub skipped: Vec<String>,
    /// Always true: titles and bodies are third-party text.
    pub untrusted_text: bool,
}

impl Digest {
    /// True if nothing changed since `since` (upcoming work does not count).
    pub fn no_changes(&self) -> bool {
        self.new_announcements.is_empty()
            && self.new_assignments.is_empty()
            && self.changed_assignments.is_empty()
            && self.new_files.is_empty()
            && self.new_recordings.is_empty()
    }
}

/// What the previous digest saw. Stored as JSON.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct State {
    /// When that digest was made.
    pub at: Option<DateTime<Utc>>,
    /// Recording ids already reported or present.
    #[serde(default)]
    pub recordings: BTreeSet<String>,
    /// Assignment id to (due date, points).
    #[serde(default)]
    pub assignments: BTreeMap<u64, (Option<DateTime<Utc>>, Option<f64>)>,
    /// File id to size.
    #[serde(default)]
    pub files: BTreeMap<u64, u64>,
    /// Courses whose recordings were checked; others keep old entries.
    #[serde(default)]
    pub recording_courses: BTreeSet<u64>,
}

/// Everything fetched for one course.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CourseData {
    /// The course.
    pub course: Option<Course>,
    /// Its assignments.
    pub assignments: Vec<Assignment>,
    /// Its files.
    pub files: Vec<CanvasFile>,
    /// Its recordings.
    pub recordings: Vec<RecordingSummary>,
    /// Whether recordings were actually checked for this course.
    pub recordings_checked: bool,
}

fn label(course: &Course) -> String {
    course
        .course_code
        .as_deref()
        .unwrap_or(&course.name)
        .trim()
        .to_owned()
}

fn wrap<T>(course: &Course, item: T) -> InCourse<T> {
    InCourse {
        course_id: course.id,
        course: label(course),
        item,
    }
}

/// Filters fetched data into a digest, comparing with `previous` when
/// there is one. Returns the digest and the state to save. Pure.
pub fn build(
    courses: Vec<CourseData>,
    announcements: Vec<Announcement>,
    options: &Options,
    previous: Option<&State>,
    now: DateTime<Utc>,
) -> (Digest, State) {
    let since = options.since;
    let after = |t: Option<DateTime<Utc>>| t.is_some_and(|t| t > since);
    let mut d = Digest {
        since: Some(since),
        generated_at: Some(now),
        untrusted_text: true,
        new_announcements: announcements
            .into_iter()
            .filter(|a| after(a.posted_at))
            .collect(),
        ..Default::default()
    };
    let mut state = State {
        at: Some(now),
        ..Default::default()
    };
    if let Some(prev) = previous {
        // Keep what we could not re-check this time, so nothing reappears.
        state.recordings = prev.recordings.clone();
        state.recording_courses = prev.recording_courses.clone();
        if !options.files {
            state.files = prev.files.clone();
        }
    }
    let horizon = now + options.upcoming;
    for data in courses {
        let Some(course) = data.course else { continue };
        for a in data.assignments {
            if !a.is_submitted() && a.due_at.is_some_and(|due| due >= now && due <= horizon) {
                d.upcoming.push(wrap(&course, a.clone()));
            }
            let fingerprint = (a.due_at, a.points_possible);
            match previous.map(|p| p.assignments.get(&a.id)) {
                // First run: fall back to timestamps.
                None if after(a.created_at) => d.new_assignments.push(wrap(&course, a.clone())),
                None if after(a.updated_at) => d.changed_assignments.push(wrap(&course, a.clone())),
                None => {}
                Some(None) => d.new_assignments.push(wrap(&course, a.clone())),
                Some(Some(old)) if *old != fingerprint => {
                    d.changed_assignments.push(wrap(&course, a.clone()))
                }
                Some(Some(_)) => {}
            }
            state.assignments.insert(a.id, fingerprint);
        }
        if options.files {
            for f in data.files {
                let news = match previous {
                    None => after(f.updated_at),
                    Some(p) => p.files.get(&f.id) != Some(&f.size),
                };
                state.files.insert(f.id, f.size);
                if news {
                    d.new_files.push(wrap(&course, f));
                }
            }
        }
        if data.recordings_checked {
            let known = previous.is_some_and(|p| p.recording_courses.contains(&course.id));
            for r in data.recordings {
                let news = if known {
                    !state.recordings.contains(&r.id)
                } else {
                    after(r.start_time)
                };
                state.recordings.insert(r.id.clone());
                if news {
                    d.new_recordings.push(wrap(&course, r));
                }
            }
            state.recording_courses.insert(course.id);
        }
    }
    d.new_announcements
        .sort_by_key(|a| std::cmp::Reverse(a.posted_at));
    d.upcoming.sort_by_key(|u| u.item.due_at);
    (d, state)
}

/// Fetches every active course and builds the digest against `previous`.
/// Returns the digest and the state to save with [`save_state`].
///
/// # Errors
/// Fails only if the course list or announcements cannot be read, or the
/// Canvas session expired. Per-course and ReCap problems are recorded in
/// [`Digest::skipped`]; those parts keep their previous state, so nothing
/// is lost for the next run.
pub async fn digest(
    client: &Client,
    recap: Option<&Panopto>,
    options: &Options,
    previous: Option<&State>,
) -> Result<(Digest, State)> {
    let now = Utc::now();
    let courses = client.courses().await?;
    let ids: Vec<u64> = courses.iter().map(|c| c.id).collect();
    let announcements = client.announcements(&ids, options.since).await?;
    let mut skipped = Vec::new();
    let recap = match (options.recordings, recap) {
        (false, _) => None,
        (true, None) => {
            skipped.push("recordings: ReCap is not signed in".to_owned());
            None
        }
        (true, Some(r)) => match r.check_session().await {
            Ok(()) => Some(r),
            Err(e) => {
                skipped.push(format!("recordings: {e}"));
                None
            }
        },
    };
    let results: Vec<Result<(CourseData, Vec<String>)>> = futures::stream::iter(courses)
        .map(|course| async move {
            let mut notes = Vec::new();
            let assignments = client.assignments(course.id).await?;
            let files = if options.files {
                match canvas_sync::list_files(client, &course).await {
                    Ok((files, failed)) => {
                        if !failed.is_empty() {
                            notes.push(format!(
                                "files: {} unreadable in {}",
                                failed.len(),
                                label(&course)
                            ));
                        }
                        files
                    }
                    Err(e @ Error::SessionExpired) => return Err(e),
                    Err(e) => {
                        notes.push(format!("files for {}: {e}", label(&course)));
                        Vec::new()
                    }
                }
            } else {
                Vec::new()
            };
            let (recordings, recordings_checked) = match (
                recap,
                course.course_code.as_deref().and_then(panopto::module_code),
            ) {
                (Some(r), Some(code)) => {
                    let term = course.term.as_ref().map(|t| t.name.as_str());
                    match r.course_recordings(&code, term).await {
                        Ok(found) => (found.recordings, true),
                        Err(Error::NotFound(_)) => (Vec::new(), true),
                        Err(e) => {
                            notes.push(format!("recordings for {code}: {e}"));
                            (Vec::new(), false)
                        }
                    }
                }
                _ => (Vec::new(), false),
            };
            Ok((
                CourseData {
                    course: Some(course),
                    assignments,
                    files,
                    recordings,
                    recordings_checked,
                },
                notes,
            ))
        })
        .buffer_unordered(CONCURRENCY)
        .collect()
        .await;
    let mut courses = Vec::new();
    for result in results {
        match result {
            Ok((data, notes)) => {
                courses.push(data);
                skipped.extend(notes);
            }
            Err(e @ Error::SessionExpired) => return Err(e),
            Err(e) => skipped.push(format!("a course could not be read: {e}")),
        }
    }
    let (mut d, mut state) = build(courses, announcements, options, previous, now);
    // Courses that failed outright keep their previous assignment and file
    // entries, so their changes are reported next time, not lost.
    if let Some(prev) = previous {
        for (id, v) in &prev.assignments {
            state.assignments.entry(*id).or_insert(*v);
        }
        for (id, v) in &prev.files {
            state.files.entry(*id).or_insert(*v);
        }
    }
    d.skipped = skipped;
    Ok((d, state))
}

/// Resolves a `since` argument: `last` means the last successful digest
/// (or `fallback` ago if there has been none); anything else is a window
/// such as `2d`. Returns the time and whether it came from the saved state.
///
/// # Errors
/// [`Error::Input`] for an unreadable window.
pub fn resolve_since(
    text: &str,
    root: &Path,
    now: DateTime<Utc>,
    fallback: Duration,
) -> Result<(DateTime<Utc>, bool)> {
    if text.eq_ignore_ascii_case("last") {
        let at = load_state(root, now).0.and_then(|s| s.at);
        return Ok((at.unwrap_or(now - fallback), true));
    }
    let window = canvas_api::parse_window(text).map_err(Error::Input)?;
    Ok((now - window, false))
}

fn marker(root: &Path) -> PathBuf {
    root.join(".canvas-sync").join("last-digest")
}

/// Loads the previous digest's state. A marker from an older version (a
/// bare timestamp) gives a state with only the time. A corrupt or future
/// marker is ignored and explained in the returned note.
pub fn load_state(root: &Path, now: DateTime<Utc>) -> (Option<State>, Option<String>) {
    let Ok(text) = std::fs::read_to_string(marker(root)) else {
        return (None, None);
    };
    let state = serde_json::from_str::<State>(&text).ok().or_else(|| {
        let at: DateTime<Utc> = text.trim().parse().ok()?;
        Some(State {
            at: Some(at),
            ..Default::default()
        })
    });
    match state {
        Some(s) if s.at.is_some_and(|at| at > now) => (
            None,
            Some("ignored a last-digest marker dated in the future".into()),
        ),
        Some(s) => (Some(s), None),
        None => (
            None,
            Some("ignored an unreadable last-digest marker".into()),
        ),
    }
}

/// Returns when the last successful digest was made, if any.
pub fn last_run(root: &Path) -> Option<DateTime<Utc>> {
    load_state(root, Utc::now()).0.and_then(|s| s.at)
}

/// Saves `state` as the last successful digest.
///
/// # Errors
/// [`Error::Io`] if it cannot be written.
pub fn save_state(root: &Path, state: &State) -> Result<()> {
    let path = marker(root);
    let io = |source| Error::Io {
        path: path.clone(),
        source,
    };
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(io)?;
    }
    let text = serde_json::to_string(state).map_err(|e| Error::Decode {
        what: "digest state".into(),
        detail: e.to_string(),
    })?;
    // Unique temp name: concurrent CLI and MCP runs must not collide.
    let tmp = path.with_extension(format!("{}.part", std::process::id()));
    std::fs::write(&tmp, text).map_err(io)?;
    std::fs::rename(&tmp, &path).map_err(io)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn fixtures() -> (Vec<CourseData>, Vec<Announcement>) {
        let course: Course = serde_json::from_str(
            r#"{"id":1,"name":"Example Studies","course_code":"ABC1020 (26/27)"}"#,
        )
        .unwrap();
        let assignment =
            |id: u64, created: &str, updated: &str, due: &str, submitted: bool| -> Assignment {
                serde_json::from_value(serde_json::json!({
                    "id": id, "course_id": 1, "name": format!("A{id}"), "html_url": "https://x",
                    "created_at": created, "updated_at": updated, "due_at": due,
                    "submission": { "submitted_at": submitted.then_some("2026-11-04T00:00:00Z") }
                }))
                .unwrap()
            };
        let file: CanvasFile = serde_json::from_str(
            r#"{"id":5,"display_name":"Lecture 7.pptx","size":1,"updated_at":"2026-11-04T09:00:00Z","url":"https://x"}"#,
        )
        .unwrap();
        let recording = RecordingSummary {
            id: "0a1b2c3d-1111-4222-8333-444455556666".into(),
            title: "ABC1020/L01/01".into(),
            folder: "2026/27 - ABC1020".into(),
            start_time: Some(t("2026-11-04T11:35:00Z")),
            duration_secs: 6000.0,
            has_captions: true,
            untrusted_text: true,
        };
        let data = CourseData {
            course: Some(course),
            assignments: vec![
                assignment(
                    1,
                    "2026-11-04T10:00:00Z",
                    "2026-11-04T10:00:00Z",
                    "2026-11-08T12:00:00Z",
                    false,
                ),
                assignment(
                    2,
                    "2026-10-01T10:00:00Z",
                    "2026-11-04T10:00:00Z",
                    "2026-11-20T12:00:00Z",
                    false,
                ),
                assignment(
                    3,
                    "2026-10-01T10:00:00Z",
                    "2026-10-01T10:00:00Z",
                    "2026-11-07T12:00:00Z",
                    true,
                ),
            ],
            files: vec![file],
            recordings: vec![recording],
            recordings_checked: true,
        };
        (vec![data], Vec::new())
    }

    fn options(since: &str) -> Options {
        Options {
            since: t(since),
            upcoming: Duration::days(7),
            files: true,
            recordings: true,
        }
    }

    #[test]
    fn first_run_reports_changes_and_upcoming() {
        let (courses, ann) = fixtures();
        let (d, _) = build(
            courses,
            ann,
            &options("2026-11-03T00:00:00Z"),
            None,
            t("2026-11-05T12:00:00Z"),
        );
        assert_eq!(
            d.new_assignments
                .iter()
                .map(|a| a.item.id)
                .collect::<Vec<_>>(),
            [1]
        );
        assert_eq!(
            d.changed_assignments
                .iter()
                .map(|a| a.item.id)
                .collect::<Vec<_>>(),
            [2]
        );
        assert_eq!(d.new_files.len(), 1);
        assert_eq!(d.new_recordings.len(), 1);
        assert_eq!(
            d.upcoming.iter().map(|a| a.item.id).collect::<Vec<_>>(),
            [1]
        );
        assert!(!d.no_changes());
    }

    #[test]
    fn back_to_back_runs_give_an_empty_second_digest() {
        let now = t("2026-11-05T12:00:00Z");
        let (courses, ann) = fixtures();
        let (_, state) = build(courses, ann, &options("2026-11-03T00:00:00Z"), None, now);
        let (courses, ann) = fixtures();
        let mut opts = options("2026-11-03T00:00:00Z");
        opts.since = state.at.unwrap();
        let (second, _) = build(
            courses,
            ann,
            &opts,
            Some(&state),
            now + Duration::minutes(1),
        );
        assert!(second.no_changes(), "{second:?}");
        assert_eq!(second.upcoming.len(), 1, "deadline reminders persist");
    }

    #[test]
    fn late_published_recording_is_still_reported() {
        let now = t("2026-11-05T12:00:00Z");
        let (mut courses, ann) = fixtures();
        let (_, state) = build(
            courses.clone(),
            ann.clone(),
            &options("2026-11-03T00:00:00Z"),
            None,
            now,
        );
        // A recording dated before the last run appears only now.
        let late = RecordingSummary {
            id: "1b2c3d4e-2222-4333-8444-555566667777".into(),
            start_time: Some(t("2026-11-02T10:00:00Z")),
            ..courses[0].recordings[0].clone()
        };
        courses[0].recordings.push(late);
        let mut opts = options("2026-11-03T00:00:00Z");
        opts.since = now;
        let (d, _) = build(courses, ann, &opts, Some(&state), now + Duration::days(1));
        assert_eq!(d.new_recordings.len(), 1);
        assert_eq!(
            d.new_recordings[0].item.id,
            "1b2c3d4e-2222-4333-8444-555566667777"
        );
    }

    #[test]
    fn only_real_assignment_changes_count() {
        let now = t("2026-11-05T12:00:00Z");
        let (mut courses, ann) = fixtures();
        let (_, state) = build(
            courses.clone(),
            ann.clone(),
            &options("2026-11-03T00:00:00Z"),
            None,
            now,
        );
        let mut opts = options("2026-11-03T00:00:00Z");
        opts.since = now;
        // Unrelated edit: updated_at bumped, due date unchanged.
        courses[0].assignments[2].updated_at = Some(now + Duration::hours(1));
        // Real change: the deadline moved.
        courses[0].assignments[1].due_at = Some(t("2026-11-21T12:00:00Z"));
        let (d, _) = build(courses, ann, &opts, Some(&state), now + Duration::days(1));
        assert_eq!(
            d.changed_assignments
                .iter()
                .map(|a| a.item.id)
                .collect::<Vec<_>>(),
            [2]
        );
    }

    #[test]
    fn state_round_trips_and_ignores_future_or_corrupt_markers() {
        let root = std::env::temp_dir().join(format!("canvas-digest-{}", std::process::id()));
        let now = t("2026-11-05T12:00:00Z");
        assert_eq!(load_state(&root, now), (None, None));
        let state = State {
            at: Some(now),
            ..Default::default()
        };
        save_state(&root, &state).unwrap();
        assert_eq!(load_state(&root, now).0, Some(state));
        assert!(
            load_state(&root, now - Duration::days(1)).1.is_some(),
            "future marker ignored"
        );
        std::fs::write(marker(&root), "2026-11-01T00:00:00Z").unwrap();
        assert_eq!(
            load_state(&root, now).0.unwrap().at,
            Some(t("2026-11-01T00:00:00Z")),
            "old format read"
        );
        std::fs::write(marker(&root), "garbage").unwrap();
        assert!(load_state(&root, now).1.is_some());
        std::fs::remove_dir_all(&root).unwrap();
    }
}
