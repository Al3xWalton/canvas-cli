//! Typed Canvas records. Only fields Canvas CLI uses are kept.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// The signed-in Canvas user, from `GET /api/v1/users/self`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct User {
    /// Canvas user id.
    pub id: u64,
    /// Display name.
    pub name: String,
    /// Login id, often the university username or email.
    #[serde(default)]
    pub login_id: Option<String>,
}

/// An academic term, embedded in a course with `include[]=term`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Term {
    /// Term id.
    pub id: u64,
    /// Term name, e.g. `2026/27`.
    pub name: String,
}

/// A course the user is enrolled in, from `GET /api/v1/courses`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Course {
    /// Canvas course id.
    pub id: u64,
    /// Full name, e.g. `Example Studies - ABC1020`. Empty for courses
    /// hidden by date.
    #[serde(default)]
    pub name: String,
    /// Short code, e.g. `ABC1020 (26/27)`.
    #[serde(default)]
    pub course_code: Option<String>,
    /// The course's term, when requested.
    #[serde(default)]
    pub term: Option<Term>,
    /// Course start, if set.
    #[serde(default)]
    pub start_at: Option<DateTime<Utc>>,
    /// Course end, if set.
    #[serde(default)]
    pub end_at: Option<DateTime<Utc>>,
    /// True when Canvas hides the course's details until its dates open.
    #[serde(default, skip_serializing)]
    pub access_restricted_by_date: bool,
}

/// The user's own submission for an assignment, with `include[]=submission`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Submission {
    /// When it was submitted, or `None` if not yet.
    #[serde(default)]
    pub submitted_at: Option<DateTime<Utc>>,
    /// Canvas state: `unsubmitted`, `submitted`, `graded` or `pending_review`.
    #[serde(default)]
    pub workflow_state: Option<String>,
    /// Whether Canvas marked it late.
    #[serde(default)]
    pub late: bool,
    /// Whether Canvas marked it missing.
    #[serde(default)]
    pub missing: bool,
}

/// An assignment, from `GET /api/v1/courses/:id/assignments`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Assignment {
    /// Canvas assignment id.
    pub id: u64,
    /// Course the assignment belongs to.
    pub course_id: u64,
    /// Assignment title.
    pub name: String,
    /// Due time, or `None` if undated.
    #[serde(default)]
    pub due_at: Option<DateTime<Utc>>,
    /// Time after which submissions close, if set.
    #[serde(default)]
    pub lock_at: Option<DateTime<Utc>>,
    /// Maximum points, if graded on points.
    #[serde(default)]
    pub points_possible: Option<f64>,
    /// Accepted submission types, e.g. `online_upload`.
    #[serde(default)]
    pub submission_types: Vec<String>,
    /// Link to the assignment in the Canvas web UI.
    pub html_url: String,
    /// The user's submission, when requested.
    #[serde(default)]
    pub submission: Option<Submission>,
}

impl Assignment {
    /// Returns true if the user has submitted this assignment.
    pub fn is_submitted(&self) -> bool {
        self.submission
            .as_ref()
            .is_some_and(|s| s.submitted_at.is_some())
    }
}
