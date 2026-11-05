//! Browser-session sign-in for Canvas and Panopto.
//!
//! the university students cannot create Canvas API tokens, so Canvas CLI
//! borrows a real browser session. `canvas auth login` opens Chrome, the
//! user completes SSO and MFA, and this crate captures the Canvas and
//! Panopto cookies into the OS keychain. Nothing here writes secrets to
//! disk, logs or output. The MCP server only *reads* the stored session
//! and never signs in itself.
//!
//! Status: scaffold. Sign-in lands in issue #2.
