//! Filing recordings into a student's own lecture folders.
//!
//! Many students keep one folder per lecture, e.g.
//! `ABC1002 - Second Example/Lectures/L01 - Course Overview`.
//! Panopto titles do not number lectures (a module's recordings are often
//! all `ABC1002/L01/01`, the timetable slot), so recordings are paired
//! with folders **by order**: the oldest recording goes to the
//! lowest-numbered folder.
//!
//! Order alone is fragile: one missed or extra recording shifts every pair
//! after it. So each pair gets a [`title_score`] comparing the folder's
//! title with the recording's AI chapter headings, and weak pairs are
//! flagged for the user to check. [`plan_filing`] is pure, so the matching
//! is tested without a network; the CLI shows the plan and writes nothing
//! until the user passes `--apply`.

use std::path::{Path, PathBuf};

use canvas_core::{Error, Result};
use serde::Serialize;

use crate::RecordingSummary;

/// Below this [`title_score`] a pair is flagged for checking.
pub const WEAK_SCORE: f64 = 0.2;

/// A lecture folder such as `L03 - Politics and Business Intertwinted`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct LectureFolder {
    /// Lecture number from the `Lnn` prefix.
    pub number: u32,
    /// The title after the prefix (may be empty).
    pub title: String,
    /// The folder's full name, used for file names.
    pub name: String,
    /// Where it is.
    pub path: PathBuf,
}

/// One folder paired with one recording.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Pairing {
    /// The folder.
    pub folder: LectureFolder,
    /// The recording.
    pub recording: RecordingSummary,
    /// Agreement between the folder title and the recording's chapters,
    /// 0 to 1, or `None` when either side has nothing to compare.
    pub score: Option<f64>,
}

impl Pairing {
    /// True if the chapters clearly disagree with the folder title.
    pub fn is_weak(&self) -> bool {
        self.score.is_some_and(|s| s < WEAK_SCORE)
    }
}

/// The pairing of folders with recordings.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct FilingPlan {
    /// Pairs in lecture order.
    pub pairs: Vec<Pairing>,
    /// Folders with no recording (more folders than recordings).
    pub folders_without_recording: Vec<LectureFolder>,
    /// Recordings with no folder (more recordings than folders).
    pub recordings_without_folder: Vec<RecordingSummary>,
}

/// Lists the `Lnn ...` folders directly inside `dir`, by lecture number.
///
/// Accepts `L1`, `L01 - Title`, `l02: Title`, `L3 Title`. Other entries are
/// ignored.
///
/// # Errors
/// [`Error::Io`] if `dir` cannot be read; [`Error::Input`] if two folders
/// share a number.
pub fn scan_lecture_folders(dir: &Path) -> Result<Vec<LectureFolder>> {
    let entries = std::fs::read_dir(dir).map_err(|source| Error::Io {
        path: dir.to_owned(),
        source,
    })?;
    let mut out: Vec<LectureFolder> = entries
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let (number, title) = parse_folder_name(&name)?;
            Some(LectureFolder {
                number,
                title,
                name,
                path: e.path(),
            })
        })
        .collect();
    out.sort_by_key(|f| f.number);
    if let Some(w) = out.windows(2).find(|w| w[0].number == w[1].number) {
        return Err(Error::Input(format!(
            "two folders are lecture {}: `{}` and `{}`",
            w[0].number, w[0].name, w[1].name
        )));
    }
    Ok(out)
}

fn parse_folder_name(name: &str) -> Option<(u32, String)> {
    let rest = name.strip_prefix(['L', 'l'])?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    let number = digits.parse().ok()?;
    let title = rest[digits.len()..]
        .trim_start_matches(|c: char| c.is_whitespace() || matches!(c, '-' | '–' | '—' | ':' | '.'))
        .trim()
        .to_owned();
    Some((number, title))
}

/// Words too common to say anything about a lecture's topic.
const STOP: &[&str] = &[
    "and",
    "the",
    "for",
    "with",
    "from",
    "into",
    "that",
    "this",
    "about",
    "your",
    "their",
    "what",
    "lecture",
    "course",
    "module",
    "week",
    "part",
    "intro",
    "matter",
    "matters",
    // Generic titles say nothing checkable about the topic.
    "overview",
    "introduction",
    "summary",
    "review",
    "recap",
    "revision",
    "welcome",
];

fn words(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .map(str::to_lowercase)
        .filter(|w| w.len() >= 4 && !STOP.contains(&w.as_str()))
        .collect()
}

/// Returns the share of the folder title's meaningful words (by a
/// five-letter stem, so `globalisation` matches `global`) that appear in
/// the chapter headings, or `None` if either has no meaningful words.
///
/// # Example
/// ```
/// let chapters = ["Globalization And Business", "PESTEL Framework"];
/// assert_eq!(panopto::title_score("Globalisation and Business Environments", &chapters), Some(2.0 / 3.0));
/// assert_eq!(panopto::title_score("", &chapters), None);
/// ```
pub fn title_score(title: &str, chapters: &[&str]) -> Option<f64> {
    let title = words(title);
    let text: Vec<String> = chapters.iter().flat_map(|c| words(c)).collect();
    if title.is_empty() || text.is_empty() {
        return None;
    }
    let stem = |w: &str| w.chars().take(5).collect::<String>();
    let found = title
        .iter()
        .filter(|t| text.iter().any(|c| stem(c) == stem(t)))
        .count();
    Some(found as f64 / title.len() as f64)
}

/// Pairs folders (by lecture number) with recordings (oldest first).
/// `chapters` gives each recording's chapter headings by id, for scoring.
pub fn plan_filing(
    folders: Vec<LectureFolder>,
    recordings: Vec<RecordingSummary>,
    chapters: &std::collections::HashMap<String, Vec<String>>,
) -> FilingPlan {
    // Undated recordings cannot be placed in order; never let them shift
    // the others. They are listed as unpaired.
    let (mut recordings, undated): (Vec<_>, Vec<_>) =
        recordings.into_iter().partition(|r| r.start_time.is_some());
    recordings.sort_by(|a, b| {
        a.start_time
            .cmp(&b.start_time)
            .then_with(|| a.id.cmp(&b.id))
    });
    let n = folders.len().min(recordings.len());
    let mut folders = folders.into_iter();
    let mut recordings = recordings.into_iter();
    let pairs = (&mut folders)
        .zip(&mut recordings)
        .take(n)
        .map(|(folder, recording)| {
            let heads: Vec<&str> = chapters
                .get(&recording.id)
                .map(|v| v.iter().map(String::as_str).collect())
                .unwrap_or_default();
            let score = title_score(&folder.title, &heads);
            Pairing {
                folder,
                recording,
                score,
            }
        })
        .collect();
    FilingPlan {
        pairs,
        folders_without_recording: folders.collect(),
        recordings_without_folder: recordings.chain(undated).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(id: &str, day: u32) -> RecordingSummary {
        RecordingSummary {
            id: id.into(),
            title: "ABC1002/L01/01".into(),
            folder: "2026/27 - ABC1002".into(),
            start_time: format!("2026-10-{day:02}T14:35:00Z").parse().ok(),
            duration_secs: 3600.0,
            has_captions: true,
            untrusted_text: true,
        }
    }

    fn folder(n: u32, title: &str) -> LectureFolder {
        LectureFolder {
            number: n,
            title: title.into(),
            name: format!("L{n:02} - {title}"),
            path: PathBuf::new(),
        }
    }

    #[test]
    fn folder_names_parse() {
        assert_eq!(
            parse_folder_name("L01 - Course Overview"),
            Some((1, "Course Overview".into()))
        );
        assert_eq!(
            parse_folder_name("l3: Politics"),
            Some((3, "Politics".into()))
        );
        assert_eq!(parse_folder_name("L12"), Some((12, String::new())));
        assert_eq!(parse_folder_name("Lab 1"), None);
        assert_eq!(parse_folder_name("Seminars"), None);
    }

    #[test]
    fn pairs_by_order_and_reports_leftovers() {
        let folders = vec![
            folder(2, "Globalisation"),
            folder(1, "Course Overview"),
            folder(3, "Politics"),
        ];
        let mut folders = folders;
        folders.sort_by_key(|f| f.number);
        // Given newest first, as Panopto lists them.
        let recordings = vec![rec("c", 20), rec("a", 6), rec("b", 13)];
        let plan = plan_filing(folders.clone(), recordings, &Default::default());
        let got: Vec<(u32, &str)> = plan
            .pairs
            .iter()
            .map(|p| (p.folder.number, p.recording.id.as_str()))
            .collect();
        assert_eq!(got, [(1, "a"), (2, "b"), (3, "c")]);

        let plan = plan_filing(folders.clone(), vec![rec("a", 6)], &Default::default());
        assert_eq!(plan.pairs.len(), 1);
        assert_eq!(plan.folders_without_recording.len(), 2);

        let plan = plan_filing(
            vec![folders[0].clone()],
            vec![rec("a", 6), rec("b", 13)],
            &Default::default(),
        );
        assert_eq!(
            plan.recordings_without_folder
                .iter()
                .map(|r| r.id.as_str())
                .collect::<Vec<_>>(),
            ["b"]
        );
    }

    #[test]
    fn generic_titles_are_not_checkable() {
        assert_eq!(
            title_score("Course Overview", &["Shareholder Theory"]),
            None
        );
        assert_eq!(title_score("Introduction", &["Anything"]), None);
    }

    #[test]
    fn undated_recordings_never_shift_the_order() {
        let mut undated = rec("x", 1);
        undated.start_time = None;
        let plan = plan_filing(
            vec![folder(1, "A"), folder(2, "B")],
            vec![undated, rec("a", 6), rec("b", 13)],
            &Default::default(),
        );
        let got: Vec<&str> = plan.pairs.iter().map(|p| p.recording.id.as_str()).collect();
        assert_eq!(got, ["a", "b"]);
        assert_eq!(plan.recordings_without_folder[0].id, "x");
    }

    #[test]
    fn weak_pairs_are_flagged() {
        let chapters = std::collections::HashMap::from([
            (
                "a".to_owned(),
                vec!["Introduction To Second Example".to_owned()],
            ),
            ("b".to_owned(), vec!["Shareholder Theory".to_owned()]),
        ]);
        let plan = plan_filing(
            vec![
                folder(1, "Global Trade Overview"),
                folder(2, "Cultures and Societies"),
            ],
            vec![rec("a", 1), rec("b", 2)],
            &chapters,
        );
        assert!(!plan.pairs[0].is_weak());
        assert!(plan.pairs[1].is_weak());
    }
}
