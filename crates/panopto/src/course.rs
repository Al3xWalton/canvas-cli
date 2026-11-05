//! Joining Canvas courses to their ReCap recordings.
//!
//! Shared by the CLI and the MCP server so both resolve "a recording or a
//! course" the same way.

use std::path::{Path, PathBuf};

use canvas_core::{Error, Result, sanitize};

use crate::{Panopto, delivery_id, module_code};

/// What a `recap` target resolved to.
#[derive(Debug, Clone, PartialEq)]
pub struct RecapTargets {
    /// Folder files for these recordings go in.
    pub dir: PathBuf,
    /// Delivery IDs, newest first for a course.
    pub ids: Vec<String>,
    /// Set when the course match was broader than asked (every year).
    pub warning: Option<String>,
}

/// Resolves `target`: a recording URL or delivery ID goes to
/// `<root>/ReCap`; anything else is a Canvas course (id, code or name)
/// whose newest `latest` recordings go to `<root>/<course>/ReCap`.
///
/// # Errors
/// [`Error::Input`] if the course has no module code; otherwise as for
/// [`canvas_api::resolve_course`] and [`Panopto::course_recordings`].
pub async fn recap_targets(
    canvas: &canvas_api::Client,
    recap: &Panopto,
    target: &str,
    root: &Path,
    latest: Option<usize>,
) -> Result<RecapTargets> {
    if let Ok(id) = delivery_id(target) {
        return Ok(RecapTargets {
            dir: root.join("ReCap"),
            ids: vec![id],
            warning: None,
        });
    }
    let course = canvas_api::resolve_course(canvas, target).await?;
    let code = course
        .course_code
        .as_deref()
        .and_then(module_code)
        .ok_or_else(|| {
            Error::Input(format!(
                "course {} has no module code to find its ReCap folder",
                course.id
            ))
        })?;
    let term = course.term.as_ref().map(|t| t.name.as_str());
    let found = recap.course_recordings(&code, term).await?;
    let dir = root
        .join(sanitize(
            course.course_code.as_deref().unwrap_or(&course.name),
        ))
        .join("ReCap");
    let ids = found
        .recordings
        .into_iter()
        .take(latest.unwrap_or(usize::MAX))
        .map(|r| r.id)
        .collect();
    Ok(RecapTargets {
        dir,
        ids,
        warning: found.warning,
    })
}
