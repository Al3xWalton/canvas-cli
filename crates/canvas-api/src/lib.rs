//! A read-only client for the Canvas REST API (`/api/v1`).
//!
//! [`Client`] sends the browser session from `canvas-auth` as a `Cookie`
//! header. Canvas accepts a signed-in web session for GET requests, which
//! is how its own web pages load data, so no API token is needed. It only
//! makes GET requests: Canvas CLI never changes anything on Canvas.
//!
//! List endpoints follow Canvas's `Link: <...>; rel="next"` headers until
//! the last page, and only to the Canvas host, so cookies never leak to
//! another origin. A 429, or a 403 that says the rate limit was hit, is
//! retried with exponential backoff (honouring `Retry-After`).
//!
//! A 401, or a redirect to the login page, means the session has expired.
//! Both map to [`canvas_core::Error::SessionExpired`] so the caller can tell
//! the user to run `canvas auth login`.

mod client;
mod queries;
mod types;

pub use client::Client;
pub use queries::{UpcomingAssignment, parse_window, upcoming_assignments};
pub use types::{Assignment, Course, Submission, Term, User};
