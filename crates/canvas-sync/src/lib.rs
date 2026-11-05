//! Incremental download of Canvas files to a local folder.
//!
//! A manifest of file ids and `updated_at` times lets a repeat sync skip
//! unchanged files. Every write goes to a temp file first and is renamed
//! into place, and every path stays inside the configured download root.
//!
//! Status: scaffold. Sync lands in issue #8.
