//! Questions agents ask, built from several API calls.
//!
//! These live in the library, not the CLI, so the MCP server answers them
//! identically.

use chrono::{DateTime, Duration, Utc};
use futures::{StreamExt, TryStreamExt};
use serde::Serialize;

use canvas_core::{Error, Result};

use crate::{Assignment, Client, Course};

/// How many courses are fetched at once, to stay well inside rate limits.
const CONCURRENCY: usize = 4;

/// An assignment that is due soon, with its course for context.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct UpcomingAssignment {
    /// Course short code, or its name if it has none.
    pub course: String,
    /// The assignment.
    #[serde(flatten)]
    pub assignment: Assignment,
    /// Whether the user has already submitted it.
    pub submitted: bool,
}

/// Returns assignments due between `now` and `now + within` across every
/// active course (or only `course_id`), soonest first.
///
/// # Errors
/// Fails if any course's assignments cannot be read. A partial list is
/// never returned, so an agent cannot mistake a failure for "nothing due".
pub async fn upcoming_assignments(
    client: &Client,
    within: Duration,
    now: DateTime<Utc>,
    course_id: Option<u64>,
) -> Result<Vec<UpcomingAssignment>> {
    let courses: Vec<Course> = client
        .courses()
        .await?
        .into_iter()
        .filter(|c| course_id.is_none_or(|id| c.id == id))
        .collect();
    let per_course: Vec<(Course, Vec<Assignment>)> = futures::stream::iter(courses)
        .map(|course| async move {
            let assignments = client.assignments(course.id).await?;
            Ok::<_, Error>((course, assignments))
        })
        .buffer_unordered(CONCURRENCY)
        .try_collect()
        .await?;
    let mut out = filter_upcoming(per_course, within, now);
    out.sort_by_key(|u| (u.assignment.due_at, u.assignment.course_id, u.assignment.id));
    Ok(out)
}

fn filter_upcoming(
    per_course: Vec<(Course, Vec<Assignment>)>,
    within: Duration,
    now: DateTime<Utc>,
) -> Vec<UpcomingAssignment> {
    let end = now + within;
    per_course
        .into_iter()
        .flat_map(|(course, assignments)| {
            let label = course
                .course_code
                .clone()
                .unwrap_or_else(|| course.name.clone());
            assignments
                .into_iter()
                .filter(move |a| a.due_at.is_some_and(|d| d >= now && d <= end))
                .map(move |a| UpcomingAssignment {
                    course: label.clone(),
                    submitted: a.is_submitted(),
                    assignment: a,
                })
        })
        .collect()
}

/// Parses a window such as `7d`, `48h`, `2w` or `90m`.
///
/// # Errors
/// Returns a message describing the accepted forms.
///
/// # Example
/// ```
/// assert_eq!(canvas_api::parse_window("2w").unwrap(), chrono::Duration::days(14));
/// assert!(canvas_api::parse_window("soon").is_err());
/// ```
pub fn parse_window(text: &str) -> std::result::Result<Duration, String> {
    let bad = || format!("invalid window `{text}`: use a number and m, h, d or w, e.g. 7d");
    let unit = text.chars().last().ok_or_else(bad)?;
    let num = &text[..text.len() - unit.len_utf8()];
    let n: i64 = num.parse().map_err(|_| bad())?;
    if n <= 0 || n > 3650 {
        return Err(bad());
    }
    match unit {
        'm' => Ok(Duration::minutes(n)),
        'h' => Ok(Duration::hours(n)),
        'd' => Ok(Duration::days(n)),
        'w' => Ok(Duration::weeks(n)),
        _ => Err(bad()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assignment(id: u64, due: Option<&str>, submitted: bool) -> Assignment {
        serde_json::from_value(serde_json::json!({
            "id": id, "course_id": 9, "name": format!("A{id}"),
            "due_at": due, "html_url": "https://x/a",
            "submission": { "submitted_at": submitted.then_some("2026-11-01T00:00:00Z") }
        }))
        .unwrap()
    }

    #[test]
    fn keeps_only_assignments_due_inside_the_window() {
        let now: DateTime<Utc> = "2026-11-05T12:00:00Z".parse().unwrap();
        let course: Course = serde_json::from_value(
            serde_json::json!({"id": 9, "name": "Example Studies", "course_code": "ABC1020"}),
        )
        .unwrap();
        let list = vec![
            assignment(1, Some("2026-11-04T12:00:00Z"), false),
            assignment(2, Some("2026-11-06T12:00:00Z"), true),
            assignment(3, Some("2026-11-20T12:00:00Z"), false),
            assignment(4, None, false),
        ];
        let out = filter_upcoming(vec![(course, list)], Duration::days(7), now);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].assignment.id, 2);
        assert_eq!(out[0].course, "ABC1020");
        assert!(out[0].submitted);
    }

    #[test]
    fn window_parsing_rejects_nonsense() {
        assert_eq!(parse_window("48h").unwrap(), Duration::hours(48));
        for bad in ["", "d", "0d", "-1d", "7x", "99999d", "7é", "é"] {
            assert!(parse_window(bad).is_err(), "{bad}");
        }
    }
}
