//! Typed Canvas records. Only fields Canvas CLI uses are kept.

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
