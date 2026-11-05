//! Course files and folders.
//!
//! Many universities refuse the Files list to students
//! with a 403 while still linking files from modules. Callers should
//! treat [`Client::files`] as optional and fall back to module `File`
//! items, fetching each with [`Client::file`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use canvas_core::Result;

use crate::Client;

/// A file's metadata. `url` is a short-lived download link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanvasFile {
    /// File id.
    pub id: u64,
    /// Name shown in Canvas.
    #[serde(default)]
    pub display_name: String,
    /// Size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Last change in Canvas; used to skip unchanged files.
    #[serde(default)]
    pub updated_at: Option<DateTime<Utc>>,
    /// Folder the file lives in.
    #[serde(default)]
    pub folder_id: Option<u64>,
    /// MIME type.
    #[serde(default, rename = "content-type")]
    pub content_type: Option<String>,
    /// Download link (includes a verifier). Not printed in text output.
    #[serde(default)]
    pub url: String,
    /// True if the file exists but is locked for this user.
    #[serde(default)]
    pub locked_for_user: bool,
}

/// A course folder, used to place files on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Folder {
    /// Folder id.
    pub id: u64,
    /// Path inside the course, e.g. `course files/Lectures/Week 1`.
    #[serde(default)]
    pub full_name: String,
}

impl Client {
    /// Lists every file in a course.
    ///
    /// # Errors
    /// As for [`Client::get_all`]; [`canvas_core::Error::Forbidden`] when
    /// the course hides its Files list from the user.
    pub async fn files(&self, course_id: u64) -> Result<Vec<CanvasFile>> {
        self.get_all(&format!("/api/v1/courses/{course_id}/files?per_page=100"))
            .await
    }

    /// Fetches one file's metadata.
    ///
    /// # Errors
    /// As for [`Client::get`].
    pub async fn file(&self, course_id: u64, file_id: u64) -> Result<CanvasFile> {
        self.get(&format!("/api/v1/courses/{course_id}/files/{file_id}"))
            .await
    }

    /// Lists a course's folders.
    ///
    /// # Errors
    /// As for [`Client::get_all`].
    pub async fn folders(&self, course_id: u64) -> Result<Vec<Folder>> {
        self.get_all(&format!("/api/v1/courses/{course_id}/folders?per_page=100"))
            .await
    }
}
