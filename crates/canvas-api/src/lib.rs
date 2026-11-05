//! A read-only client for the Canvas REST API (`/api/v1`).
//!
//! [`Client`] sends the browser session from `canvas-auth` as a `Cookie`
//! header. Canvas accepts a signed-in web session for GET requests, which
//! is how its own web pages load data, so no API token is needed. It only
//! makes GET requests: Canvas CLI never changes anything on Canvas.
//!
//! A 401, or a redirect to the login page, means the session has expired.
//! Both map to [`canvas_core::Error::SessionExpired`] so the caller can tell
//! the user to run `canvas auth login`.

mod client;
mod types;

pub use client::Client;
pub use types::User;
