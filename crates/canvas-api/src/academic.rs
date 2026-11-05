//! Calendar events, quizzes and grades.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use canvas_core::{Error, Result};

use crate::{Client, html_to_markdown};

/// Canvas accepts at most 10 `context_codes` per calendar request.
const CALENDAR_BATCH: usize = 10;

/// A calendar entry: a timetabled event or an assignment due date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CalendarEvent {
    /// Event id (assignment entries look like `assignment_123`).
    #[serde(deserialize_with = "id_as_string")]
    pub id: String,
    /// `event` or `assignment`.
    #[serde(rename = "type", default)]
    pub kind: String,
    /// Title. Third-party text.
    #[serde(default)]
    pub title: String,
    /// Start time.
    #[serde(default)]
    pub start_at: Option<DateTime<Utc>>,
    /// End time.
    #[serde(default)]
    pub end_at: Option<DateTime<Utc>>,
    /// Whether it lasts all day.
    #[serde(default)]
    pub all_day: bool,
    /// Room or place, if given.
    #[serde(default)]
    pub location_name: Option<String>,
    /// Owning context, e.g. `course_12345`.
    #[serde(default)]
    pub context_code: Option<String>,
    /// Owning context's display name.
    #[serde(default)]
    pub context_name: Option<String>,
    /// Description as Markdown. Third-party text.
    #[serde(default)]
    pub description: Option<String>,
    /// Link in the Canvas web UI.
    #[serde(default)]
    pub html_url: Option<String>,
    /// Always true: `title` and `description` are third-party text.
    #[serde(default = "yes")]
    pub untrusted_text: bool,
}

fn yes() -> bool {
    true
}

fn id_as_string<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<String, D::Error> {
    Ok(match serde_json::Value::deserialize(d)? {
        serde_json::Value::String(s) => s,
        other => other.to_string(),
    })
}

/// A classic Canvas quiz's metadata. Questions are never fetched.
///
/// New Quizzes appear as assignments with `submission_types:
/// ["external_tool"]` instead, so they show up in `assignments`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Quiz {
    /// Quiz id.
    pub id: u64,
    /// Title.
    #[serde(default)]
    pub title: String,
    /// `assignment`, `practice_quiz`, `graded_survey` or `survey`.
    #[serde(default)]
    pub quiz_type: Option<String>,
    /// When it opens.
    #[serde(default)]
    pub unlock_at: Option<DateTime<Utc>>,
    /// When it is due.
    #[serde(default)]
    pub due_at: Option<DateTime<Utc>>,
    /// When it closes.
    #[serde(default)]
    pub lock_at: Option<DateTime<Utc>>,
    /// Time limit in minutes, if any.
    #[serde(default)]
    pub time_limit: Option<u32>,
    /// Attempts allowed (`-1` means unlimited).
    #[serde(default)]
    pub allowed_attempts: Option<i32>,
    /// Number of questions.
    #[serde(default)]
    pub question_count: Option<u32>,
    /// Maximum points.
    #[serde(default)]
    pub points_possible: Option<f64>,
    /// Link in the Canvas web UI.
    #[serde(default)]
    pub html_url: Option<String>,
    /// Always true: `title` is third-party text.
    #[serde(default = "yes")]
    pub untrusted_text: bool,
}

/// The user's overall grade in one course.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CourseGrade {
    /// Course id.
    pub course_id: u64,
    /// Course code or name.
    pub course: String,
    /// Current score in percent, counting only graded work.
    pub current_score: Option<f64>,
    /// Current letter grade, if the course uses one.
    pub current_grade: Option<String>,
    /// Final score in percent, counting ungraded work as zero.
    pub final_score: Option<f64>,
    /// True when the course hides totals from students; the scores are
    /// then unknown, not zero or ungraded.
    pub hidden: bool,
}

/// The user's result for one assignment.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct AssignmentGrade {
    /// Assignment id.
    pub assignment_id: u64,
    /// Assignment name.
    pub name: String,
    /// Points scored, if graded and released.
    pub score: Option<f64>,
    /// Maximum points.
    pub points_possible: Option<f64>,
    /// Grade as Canvas displays it (points, percent or letter).
    pub grade: Option<String>,
    /// Canvas state, e.g. `graded` or `unsubmitted`.
    pub workflow_state: Option<String>,
    /// When it was graded.
    pub graded_at: Option<DateTime<Utc>>,
}

#[derive(Deserialize)]
struct CourseWithScores {
    id: u64,
    #[serde(default)]
    name: String,
    #[serde(default)]
    course_code: Option<String>,
    #[serde(default)]
    access_restricted_by_date: bool,
    #[serde(default)]
    hide_final_grades: bool,
    #[serde(default)]
    enrollments: Vec<Enrollment>,
}

#[derive(Deserialize)]
struct Enrollment {
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
    computed_current_score: Option<f64>,
    #[serde(default)]
    computed_current_grade: Option<String>,
    #[serde(default)]
    computed_final_score: Option<f64>,
}

#[derive(Deserialize)]
struct RawSubmission {
    assignment_id: u64,
    #[serde(default)]
    score: Option<f64>,
    #[serde(default)]
    grade: Option<String>,
    #[serde(default)]
    workflow_state: Option<String>,
    #[serde(default)]
    graded_at: Option<DateTime<Utc>>,
    #[serde(default)]
    assignment: Option<RawAssignmentRef>,
}

#[derive(Deserialize)]
struct RawAssignmentRef {
    #[serde(default)]
    name: String,
    #[serde(default)]
    points_possible: Option<f64>,
}

impl Client {
    /// Lists calendar events and assignment dates between `start` and
    /// `end` for `course_ids` and the user's own calendar, soonest first.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    pub async fn calendar(
        &self,
        course_ids: &[u64],
        start: DateTime<Utc>,
        end: DateTime<Utc>,
    ) -> Result<Vec<CalendarEvent>> {
        if end < start {
            return Err(Error::Input("calendar end is before its start".into()));
        }
        let me = self.current_user().await?;
        let mut contexts: Vec<String> = vec![format!("user_{}", me.id)];
        contexts.extend(course_ids.iter().map(|id| format!("course_{id}")));
        let fmt = "%Y-%m-%dT%H:%M:%SZ";
        let (start, end) = (start.format(fmt), end.format(fmt));
        let mut out: Vec<CalendarEvent> = Vec::new();
        for kind in ["event", "assignment"] {
            for batch in contexts.chunks(CALENDAR_BATCH) {
                let codes: String = batch
                    .iter()
                    .map(|c| format!("&context_codes[]={c}"))
                    .collect();
                out.extend(
                    self.get_all::<CalendarEvent>(&format!(
                        "/api/v1/calendar_events?type={kind}&start_date={start}&end_date={end}&per_page=100{codes}"
                    ))
                    .await?,
                );
            }
        }
        for event in &mut out {
            event.description = event
                .description
                .as_deref()
                .map(html_to_markdown)
                .filter(|d| !d.is_empty());
            event.untrusted_text = true;
        }
        out.sort_by(|a, b| a.start_at.cmp(&b.start_at).then_with(|| a.id.cmp(&b.id)));
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    }

    /// Lists a course's classic quizzes (metadata only).
    ///
    /// # Errors
    /// As for [`Client::get_all`]. If the course has quizzes switched off
    /// (404), returns [`Error::NotFound`] saying so, never an empty list
    /// that could hide a failure.
    pub async fn quizzes(&self, course_id: u64) -> Result<Vec<Quiz>> {
        self.get_all(&format!("/api/v1/courses/{course_id}/quizzes?per_page=100"))
            .await
            .map_err(|e| match e {
                Error::NotFound(_) => Error::NotFound(format!(
                    "course {course_id} has no classic quizzes; New Quizzes are listed by `assignments`"
                )),
                other => other,
            })
    }

    /// Returns the user's overall grade in each active course.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    pub async fn grades(&self) -> Result<Vec<CourseGrade>> {
        let courses: Vec<CourseWithScores> = self
            .get_all("/api/v1/courses?enrollment_state=active&include[]=total_scores&per_page=100")
            .await?;
        Ok(courses
            .into_iter()
            .filter(|c| !c.access_restricted_by_date)
            .map(|c| {
                let student = c.enrollments.iter().find(|e| e.kind == "student");
                CourseGrade {
                    course_id: c.id,
                    course: c
                        .course_code
                        .as_deref()
                        .unwrap_or(&c.name)
                        .trim()
                        .to_owned(),
                    current_score: student.and_then(|e| e.computed_current_score),
                    current_grade: student.and_then(|e| e.computed_current_grade.clone()),
                    final_score: student.and_then(|e| e.computed_final_score),
                    hidden: c.hide_final_grades,
                }
            })
            .collect())
    }

    /// Returns the user's result for each assignment in a course.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    pub async fn assignment_grades(&self, course_id: u64) -> Result<Vec<AssignmentGrade>> {
        let raw: Vec<RawSubmission> = self
            .get_all(&format!(
                "/api/v1/courses/{course_id}/students/submissions?student_ids[]=self&include[]=assignment&per_page=100"
            ))
            .await?;
        Ok(raw
            .into_iter()
            .map(|s| AssignmentGrade {
                assignment_id: s.assignment_id,
                name: s
                    .assignment
                    .as_ref()
                    .map(|a| a.name.clone())
                    .unwrap_or_default(),
                points_possible: s.assignment.as_ref().and_then(|a| a.points_possible),
                score: s.score,
                grade: s.grade,
                workflow_state: s.workflow_state,
                graded_at: s.graded_at,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn calendar_ids_may_be_numbers_or_strings() {
        let a: CalendarEvent =
            serde_json::from_str(r#"{"id":5,"type":"event","title":"L1"}"#).unwrap();
        let b: CalendarEvent =
            serde_json::from_str(r#"{"id":"assignment_9","type":"assignment"}"#).unwrap();
        assert_eq!((a.id.as_str(), b.id.as_str()), ("5", "assignment_9"));
        assert!(a.untrusted_text);
    }

    #[test]
    fn course_grade_reads_the_student_enrollment() {
        let c: CourseWithScores = serde_json::from_str(
            r#"{"id":1,"name":"N","course_code":"C1","enrollments":[
                {"type":"observer"},
                {"type":"student","computed_current_score":71.5,"computed_current_grade":null,"computed_final_score":40.0}]}"#,
        )
        .unwrap();
        let e = c.enrollments.iter().find(|e| e.kind == "student").unwrap();
        assert_eq!(e.computed_current_score, Some(71.5));
    }
}
