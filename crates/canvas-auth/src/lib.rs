//! Browser-session sign-in for Canvas and Panopto.
//!
//! the university students cannot create Canvas API tokens, so Canvas CLI
//! borrows a real browser session instead:
//!
//! 1. [`login`] opens a visible Chrome window at the Canvas login page.
//!    The user completes university SSO and MFA by hand.
//! 2. Once Canvas is signed in, the same window visits the Panopto ReCap
//!    host so its SSO hop sets the Panopto cookies too.
//! 3. The cookies for both hosts are captured as a [`Session`] and saved
//!    to the OS keychain with [`store`]. They never touch disk, logs or
//!    output anywhere else.
//!
//! Every other command, and the MCP server, only *reads* the stored
//! session with [`load`]. Nothing outside [`login`] ever signs in. When the
//! session expires, callers fail with
//! [`canvas_core::Error::SessionExpired`], whose message tells the user to
//! run `canvas auth login`.
//!
//! The keychain entry is `canvas-cli` / `browser-session`. Setting
//! `CANVAS_CLI_KEYCHAIN_ACCOUNT` selects another account name, which tests
//! use so they never read the real session.
//!
//! A session cookie is as powerful as the user's password. Treat a
//! [`Session`] that way: it deliberately has no `Display`, and its `Debug`
//! output redacts every cookie value.

mod browser;
mod session;

pub use browser::{LoginOptions, login};
pub use session::{Cookie, Session, delete, load, store};
