//! A read-only client for the Canvas REST API (`/api/v1`).
//!
//! [`Client`] sends the browser session from `canvas-auth` as a `Cookie`
//! header. Canvas accepts a signed-in web session for GET requests, which
//! is how its own web pages load data, so no API token is needed. It only
//! makes GET requests: Canvas CLI never changes anything on Canvas.
//!
//! List endpoints follow Canvas's `Link: <...>; rel="next"` headers until
//! the last page, and only to the Canvas host, so cookies never leak to
//! another origin. A 429 or a 403 that says the rate limit was hit is
//! retried with exponential backoff, honouring `Retry-After`. A 503
//! (Canvas busy, or still preparing a response) is retried with the short
//! backoff only, then becomes [`canvas_core::Error::Unavailable`].
//!
//! Text written on Canvas (page bodies, announcements, discussions) is
//! converted to Markdown and every record carrying it is marked
//! `untrusted_text: true`: it is third-party content an agent must treat
//! as data, never as instructions.
//!
//! A 401, or a redirect to the login page, means the session has expired.
//! Both map to [`canvas_core::Error::SessionExpired`] so the caller can tell
//! the user to run `canvas auth login`. The exception is a 401 whose body
//! says the signed-in user lacks permission (`"status":"unauthorized"`),
//! which is [`canvas_core::Error::Forbidden`].

mod academic;
mod client;
mod content;
mod discussion;
mod files;
mod leganto;
mod markdown;
mod queries;
mod readings;
mod release;
mod types;

pub use academic::{AssignmentGrade, CalendarEvent, CourseGrade, Quiz};
pub use client::Client;
pub use content::{Announcement, Discussion, Module, ModuleItem, Page, PageSummary};
pub use discussion::{DiscussionEntry, DiscussionRef, DiscussionThread, resolve_discussion};
pub use files::{CanvasFile, FileTarget, Folder, UserFile};
pub use markdown::html_to_markdown;
pub use queries::{UpcomingAssignment, parse_window, resolve_course, upcoming_assignments};
pub use readings::{
    ExternalList, Reading, ReadingSection, Readings, from_canvas, read_talis, reading_pages,
};
pub use release::{UpdateStatus, check_for_update};
pub use types::{Assignment, Course, Submission, Term, User};
