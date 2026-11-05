//! Safe file and folder names from third-party text.

/// Longest file or folder name we write, in bytes.
const MAX_NAME: usize = 150;

/// Makes one path component safe on macOS, Linux and Windows.
///
/// Replaces separators, reserved and control characters with `_`, drops
/// leading dots and trailing dots or spaces, caps the length at 150 bytes
/// on a character boundary, and never returns empty, `.` or `..`.
///
/// # Example
/// ```
/// assert_eq!(canvas_core::sanitize("../Week 1: Intro?.pdf"), "_Week 1_ Intro_.pdf");
/// assert_eq!(canvas_core::sanitize(".."), "_");
/// ```
pub fn sanitize(name: &str) -> String {
    let replaced: String = name
        .chars()
        .map(|c| {
            if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') {
                '_'
            } else {
                c
            }
        })
        .collect();
    let mut s = replaced
        .trim()
        .trim_start_matches('.')
        .trim_end_matches(['.', ' '])
        .to_owned();
    if s.len() > MAX_NAME {
        let mut end = MAX_NAME;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
    if s.is_empty() { "_".into() } else { s }
}
