//! A read-only client for the Canvas REST API (`/api/v1`).
//!
//! It sends the session from `canvas-auth`, follows Link-header
//! pagination, backs off on rate limits, and returns typed records.
//! It only makes GET requests: Canvas CLI never changes anything on
//! Canvas.
//!
//! Status: scaffold. The first endpoints land in issue #3.
