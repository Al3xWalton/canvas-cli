//! Downloading recordings to disk.
//!
//! - **MP4 (default).** Panopto's composite MP4 is a signed CDN URL, fetched
//!   without any cookie, written to `<dir>/.canvas-sync/tmp/<id>.part`,
//!   checked against the total size, then renamed into place. No ffmpeg.
//! - **Resumable.** An interrupted download keeps its `.part` file. The next
//!   run asks for the rest with `Range: bytes=<have>-` and appends only if
//!   the server answers `206` with a matching `Content-Range`; a plain `200`
//!   restarts from zero. The `.part` file is locked while in use, so two
//!   runs never write it at once. If the signed URL has expired (`403`), a
//!   fresh one is fetched once and the download continues.
//! - **Separate streams (`--streams`).** Camera and screen feeds exist only
//!   as HLS, so `ffmpeg -c copy` remuxes each to MP4 without re-encoding.
//!   ffmpeg is restricted to https (`-protocol_whitelist`), so a hostile
//!   playlist cannot make it read local files.
//! - **Idempotent.** A file already present under its final name is
//!   skipped; names come from the recording date and title, sanitised.

use std::path::{Path, PathBuf};

use canvas_core::{Error, Result, sanitize};
use reqwest::header;
use serde::Serialize;

use crate::{Panopto, Recording};

/// What to download for each recording.
#[derive(Debug, Clone, Default)]
pub struct DownloadOptions {
    /// Also save each separate stream (`camera`, `screen`) via ffmpeg.
    pub streams: bool,
    /// Skip the composite MP4.
    pub no_mp4: bool,
}

/// Files written (or found already present) for one recording.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Downloaded {
    /// Delivery ID.
    pub id: String,
    /// New files written this run.
    pub written: Vec<PathBuf>,
    /// Files that were already there.
    pub skipped: Vec<PathBuf>,
    /// Bytes written this run (MP4 only; ffmpeg output is not counted).
    pub bytes: u64,
}

/// Returns the base file name for a recording:
/// `YYYY-MM-DD <title> [<first 8 of id>]`. The id part keeps two
/// recordings with the same date and title apart.
///
/// # Example
/// ```
/// let r = panopto::Recording {
///     id: "x".into(), title: "ABC1016/L01/01".into(), folder: None,
///     start_time: "2026-10-29T14:35:00Z".parse().ok(), duration_secs: 0.0,
///     mp4_url: None, streams: vec![], has_captions: false,
///     caption_languages: vec![], chapters: vec![], untrusted_text: true,
/// };
/// assert_eq!(panopto::file_stem(&r), "2026-10-29 ABC1016_L01_01 [x]");
/// ```
pub fn file_stem(recording: &Recording) -> String {
    let date = recording
        .start_time
        .map(|t| t.format("%Y-%m-%d ").to_string())
        .unwrap_or_default();
    let title = recording
        .title
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let short: String = recording.id.chars().take(8).collect();
    sanitize(&format!("{date}{title} [{short}]"))
}

impl Panopto {
    /// Downloads `recording` into `dir` according to `options`.
    ///
    /// `progress` receives short status lines.
    ///
    /// # Errors
    /// [`Error::NotFound`] if the recording has no MP4 and streams were not
    /// requested; [`Error::Input`] if ffmpeg is needed but missing;
    /// [`Error::Http`] / [`Error::Io`] for transfer or disk problems.
    pub async fn download(
        &self,
        recording: &Recording,
        dir: &Path,
        options: &DownloadOptions,
        progress: impl Fn(&str),
    ) -> Result<Downloaded> {
        let io = |path: &Path| {
            let path = path.to_owned();
            move |source| Error::Io { path, source }
        };
        std::fs::create_dir_all(dir).map_err(io(dir))?;
        let tmp = dir.join(".canvas-sync").join("tmp");
        std::fs::create_dir_all(&tmp).map_err(io(&tmp))?;
        if !crate::is_uuid(&recording.id) {
            return Err(Error::Input(format!(
                "`{}` is not a ReCap delivery ID",
                recording.id
            )));
        }
        clean_stale(&tmp);
        let stem = file_stem(recording);
        // Finished files are only ever renamed into place, so a non-empty
        // file under the final name is complete; an empty one is redone.
        let done = |p: &Path| std::fs::metadata(p).is_ok_and(|m| m.len() > 0);
        let pid = std::process::id();
        let mut out = Downloaded {
            id: recording.id.clone(),
            ..Default::default()
        };

        if !options.no_mp4 {
            let url = recording.mp4_url.as_deref().ok_or_else(|| {
                Error::NotFound(format!(
                    "recording {} has no MP4 yet; try --streams",
                    recording.id
                ))
            })?;
            let dest = dir.join(format!("{stem}.mp4"));
            if done(&dest) {
                out.skipped.push(dest);
            } else {
                progress(&format!("downloading {stem}.mp4"));
                let part = tmp.join(format!("{}.part", recording.id));
                let result = match self.fetch_to(url, &part, &dest).await {
                    // The signed link expired: get a fresh one and resume.
                    Err(Error::Forbidden(_)) => {
                        progress("download link expired; getting a fresh one");
                        let fresh = self.recording(&recording.id).await?;
                        let url = fresh.mp4_url.ok_or_else(|| {
                            Error::NotFound(format!(
                                "recording {} no longer has an MP4",
                                recording.id
                            ))
                        })?;
                        self.fetch_to(&url, &part, &dest).await
                    }
                    other => other,
                };
                let bytes = result?;
                out.bytes += bytes;
                out.written.push(dest);
            }
        }

        if options.streams {
            ffmpeg_available().await?;
            for (i, stream) in recording.streams.iter().enumerate() {
                let label = if recording
                    .streams
                    .iter()
                    .filter(|s| s.kind == stream.kind)
                    .count()
                    > 1
                {
                    format!("{} {}", stream.kind, i + 1)
                } else {
                    stream.kind.clone()
                };
                let dest = dir.join(format!("{stem} ({label}).mp4"));
                if done(&dest) {
                    out.skipped.push(dest);
                    continue;
                }
                progress(&format!("remuxing {label} stream with ffmpeg"));
                let part = tmp.join(format!("{}-{i}-{pid}.part.mp4", recording.id));
                match remux(&stream.url, &part).await {
                    Ok(()) => {
                        std::fs::rename(&part, &dest).map_err(io(&dest))?;
                        out.written.push(dest);
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&part);
                        return Err(e);
                    }
                }
            }
        }
        Ok(out)
    }

    /// Fetches a signed CDN URL (no cookie) into `part`, resuming from
    /// whatever `part` already holds, then renames it to `dest` while still
    /// holding the lock. Returns the bytes received this call.
    ///
    /// A resume sends `If-Range` with the ETag (or Last-Modified) saved in
    /// `<part>.meta` from the first response, so if the file changed on the
    /// server it answers 200 and we restart instead of splicing two files.
    /// Without a saved validator we restart. `part` is kept on transfer
    /// errors and removed when it cannot be trusted.
    async fn fetch_to(&self, url: &str, part: &Path, dest: &Path) -> Result<u64> {
        let parsed = url::Url::parse(url).map_err(|e| Error::Http(e.to_string()))?;
        if parsed.scheme() != "https" && !cfg!(test) {
            return Err(Error::Http("ReCap MP4 URL is not https".into()));
        }
        let io = |source| Error::Io {
            path: part.to_owned(),
            source,
        };
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(part)
            .map_err(io)?;
        if file.try_lock().is_err() {
            return Err(Error::Input(
                "this recording is already being downloaded by another run".into(),
            ));
        }
        let meta = part.with_extension("part.meta");
        let validator = std::fs::read_to_string(&meta)
            .ok()
            .filter(|v| !v.trim().is_empty());
        let mut have = file.metadata().map_err(io)?.len();
        if have > 0 && validator.is_none() {
            // Cannot prove the bytes match the server's file: start over.
            file.set_len(0).map_err(io)?;
            have = 0;
        }
        let mut request = self.http.get(parsed);
        if let (true, Some(v)) = (have > 0, validator.as_deref()) {
            request = request
                .header(header::RANGE, format!("bytes={have}-"))
                .header(header::IF_RANGE, v.trim());
        }
        let mut response = request
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        let status = response.status();
        let (offset, total) = match status {
            reqwest::StatusCode::PARTIAL_CONTENT => {
                let range = response
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(content_range);
                match range {
                    Some((start, Some(total))) if start == have => (have, Some(total)),
                    _ => {
                        // Not the bytes we asked for: start over.
                        drop(file);
                        let _ = std::fs::remove_file(part);
                        return Err(Error::Http(
                            "ReCap server sent the wrong part of the file; run again to restart"
                                .into(),
                        ));
                    }
                }
            }
            reqwest::StatusCode::RANGE_NOT_SATISFIABLE => {
                // We may already have it all.
                let total = response
                    .headers()
                    .get(header::CONTENT_RANGE)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.strip_prefix("bytes */"))
                    .and_then(|v| v.parse::<u64>().ok());
                if total == Some(have) && have > 0 {
                    std::fs::rename(part, dest).map_err(|source| Error::Io {
                        path: dest.to_owned(),
                        source,
                    })?;
                    let _ = std::fs::remove_file(&meta);
                    drop(file);
                    return Ok(0);
                }
                drop(file);
                let _ = std::fs::remove_file(part);
                return Err(Error::Http(
                    "ReCap download could not resume; run again to restart".into(),
                ));
            }
            s if s.is_success() => {
                // A full body: a fresh start, a changed file, or a server
                // that ignores Range. Restart and remember its validator.
                file.set_len(0).map_err(io)?;
                let v = response
                    .headers()
                    .get(header::ETAG)
                    .or_else(|| response.headers().get(header::LAST_MODIFIED))
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or_default()
                    .to_owned();
                std::fs::write(&meta, v).map_err(io)?;
                (0, response.content_length())
            }
            reqwest::StatusCode::FORBIDDEN | reqwest::StatusCode::UNAUTHORIZED => {
                return Err(Error::Forbidden("ReCap MP4 link expired".into()));
            }
            s => return Err(Error::Http(format!("ReCap MP4 download returned {s}"))),
        };
        std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offset)).map_err(io)?;
        let mut received = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| Error::Http(e.to_string()))?
        {
            std::io::Write::write_all(&mut file, &chunk).map_err(io)?;
            received += chunk.len() as u64;
        }
        file.sync_all().map_err(io)?;
        let size = offset + received;
        let Some(total) = total else {
            return Err(Error::Http(
                "ReCap did not say how big the MP4 is, so it cannot be checked; run again".into(),
            ));
        };
        if size != total {
            if size > total {
                drop(file);
                let _ = std::fs::remove_file(part);
                let _ = std::fs::remove_file(&meta);
            }
            return Err(Error::Http(format!(
                "ReCap MP4 is {size} of {total} bytes; run again to resume"
            )));
        }
        // Rename while the lock is still held, so no other run can reopen
        // the finished part in between.
        std::fs::rename(part, dest).map_err(|source| Error::Io {
            path: dest.to_owned(),
            source,
        })?;
        let _ = std::fs::remove_file(&meta);
        drop(file);
        Ok(received)
    }
}

/// Removes temp files older than a week that no run is using, including
/// leftovers from recordings that were later deleted.
fn clean_stale(tmp: &Path) {
    const WEEK: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 3600);
    let Ok(entries) = std::fs::read_dir(tmp) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let old = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age > WEEK);
        if !old {
            continue;
        }
        // Skip files another run holds locked.
        let unlocked = std::fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .is_ok_and(|f| f.try_lock().is_ok());
        if unlocked {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Parses `Content-Range: bytes <start>-<end>/<total>` into
/// `(start, Some(total))`; an unknown total (`*`) gives `None`.
fn content_range(value: &str) -> Option<(u64, Option<u64>)> {
    let rest = value.trim().strip_prefix("bytes ")?;
    let (range, total) = rest.split_once('/')?;
    let start = range.split_once('-')?.0.trim().parse().ok()?;
    let total = total.trim().parse().ok();
    Some((start, total))
}

async fn ffmpeg_available() -> Result<()> {
    let ok = tokio::process::Command::new("ffmpeg")
        .arg("-version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .await
        .is_ok_and(|s| s.success());
    if ok {
        Ok(())
    } else {
        Err(Error::Input(
            "ffmpeg is needed for --streams; install it (e.g. `brew install ffmpeg`)".into(),
        ))
    }
}

/// Remuxes an HLS playlist to MP4 without re-encoding.
async fn remux(url: &str, dest: &Path) -> Result<()> {
    let parsed = url::Url::parse(url).map_err(|e| Error::Http(e.to_string()))?;
    if parsed.scheme() != "https" {
        return Err(Error::Http("ReCap stream URL is not https".into()));
    }
    let output = tokio::process::Command::new("ffmpeg")
        .args(["-nostdin", "-loglevel", "error", "-y"])
        .args(["-protocol_whitelist", "https,tls,tcp,crypto"])
        .arg("-i")
        .arg(parsed.as_str())
        .args(["-c", "copy", "-bsf:a", "aac_adtstoasc", "-f", "mp4"])
        .arg(PathBuf::from(dest))
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .map_err(|e| Error::Input(format!("could not run ffmpeg: {e}")))?;
    if output.status.success() {
        Ok(())
    } else {
        let tail: String = String::from_utf8_lossy(&output.stderr)
            .lines()
            .last()
            .unwrap_or("unknown error")
            .chars()
            .filter(|c| !c.is_control())
            .take(200)
            .collect();
        Err(Error::Http(format!("ffmpeg failed: {tail}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    async fn serve(body: &'static str, length: usize) -> String {
        serve_with(body, length, true, None).await
    }

    /// A stub CDN. With `ranges`, honours `Range: bytes=N-` with a 206;
    /// otherwise always sends the full body. `cut` closes the connection
    /// after that many body bytes on the first request only.
    async fn serve_with(
        body: &'static str,
        length: usize,
        ranges: bool,
        cut: Option<usize>,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            let mut first = true;
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0; 4096];
                let n = socket.read(&mut buf).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let from = request
                    .lines()
                    .find_map(|l| l.strip_prefix("range: bytes="))
                    .and_then(|r| {
                        r.trim_end_matches('-')
                            .trim()
                            .trim_end_matches('-')
                            .parse::<usize>()
                            .ok()
                    })
                    .filter(|_| ranges);
                let from = from.filter(|_| request.contains("if-range: \"v1\""));
                let (head, slice) = match from {
                    Some(start) => (
                        format!(
                            "HTTP/1.1 206 Partial Content\r\nContent-Range: bytes {start}-{}/{length}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            length - 1,
                            length - start
                        ),
                        &body[start..],
                    ),
                    None => (
                        format!(
                            "HTTP/1.1 200 OK\r\nETag: \"v1\"\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                        ),
                        body,
                    ),
                };
                let _ = socket.write_all(head.as_bytes()).await;
                let slice = match cut.filter(|_| first) {
                    Some(c) => &slice[..c.min(slice.len())],
                    None => slice,
                };
                first = false;
                let _ = socket.write_all(slice.as_bytes()).await;
            }
        });
        base
    }

    fn recap() -> Panopto {
        let config = canvas_core::Config::default();
        let session = canvas_auth::Session {
            captured_at: 0,
            cookies: vec![canvas_auth::Cookie {
                name: ".ASPXAUTH".into(),
                value: "secret".into(),
                domain: "example.hosted.panopto.com".into(),
                path: "/".into(),
                expires: None,
            }],
        };
        Panopto::new(&config, &session).unwrap()
    }

    fn recording(url: Option<String>) -> Recording {
        Recording {
            id: "0a1b2c3d-1111-4222-8333-444455556666".into(),
            title: "ABC1016/L01/01  Thu".into(),
            folder: None,
            start_time: "2026-10-29T14:35:00Z".parse().ok(),
            duration_secs: 1.0,
            mp4_url: url,
            streams: vec![],
            has_captions: false,
            caption_languages: vec![],
            chapters: vec![],
            untrusted_text: true,
        }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("panopto-dl-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[tokio::test]
    async fn downloads_once_then_skips() {
        let base = serve("video", 5).await;
        let r = recording(Some(format!("{base}/p.mp4")));
        let d = dir("once");
        let first = recap()
            .download(&r, &d, &DownloadOptions::default(), |_| {})
            .await
            .unwrap();
        assert_eq!(
            first.written,
            vec![d.join("2026-10-29 ABC1016_L01_01 Thu [0a1b2c3d].mp4")]
        );
        assert_eq!(std::fs::read(&first.written[0]).unwrap(), b"video");
        let second = recap()
            .download(&r, &d, &DownloadOptions::default(), |_| {})
            .await
            .unwrap();
        assert!(second.written.is_empty() && second.skipped.len() == 1);
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn truncated_download_leaves_nothing_behind() {
        let base = serve("vid", 5).await;
        let r = recording(Some(format!("{base}/p.mp4")));
        let d = dir("short");
        let err = recap()
            .download(&r, &d, &DownloadOptions::default(), |_| {})
            .await;
        assert!(err.is_err());
        let names: Vec<_> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from(".canvas-sync")],
            "no partial MP4 under its real name"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn empty_leftover_is_downloaded_again_and_bad_ids_refused() {
        let base = serve("video", 5).await;
        let r = recording(Some(format!("{base}/p.mp4")));
        let d = dir("empty");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join(format!("{}.mp4", file_stem(&r))), b"").unwrap();
        let got = recap()
            .download(&r, &d, &DownloadOptions::default(), |_| {})
            .await
            .unwrap();
        assert_eq!(got.written.len(), 1);
        let mut bad = r.clone();
        bad.id = "../escape".into();
        assert!(matches!(
            recap()
                .download(&bad, &d, &DownloadOptions::default(), |_| {})
                .await,
            Err(Error::Input(_))
        ));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn interrupted_download_resumes_to_the_same_bytes() {
        let body = "0123456789abcdefghij";
        let base = serve_with(body, body.len(), true, Some(8)).await;
        let r = recording(Some(format!("{base}/p.mp4")));
        let d = dir("resume");
        assert!(
            recap()
                .download(&r, &d, &DownloadOptions::default(), |_| {})
                .await
                .is_err()
        );
        let part = d.join(".canvas-sync/tmp").join(format!("{}.part", r.id));
        assert_eq!(
            std::fs::read(&part).unwrap(),
            b"01234567",
            "partial kept for resume"
        );
        let got = recap()
            .download(&r, &d, &DownloadOptions::default(), |_| {})
            .await
            .unwrap();
        assert_eq!(got.bytes, 12, "only the rest was fetched");
        assert_eq!(std::fs::read(&got.written[0]).unwrap(), body.as_bytes());
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn server_ignoring_range_restarts_cleanly() {
        let body = "0123456789abcdefghij";
        let base = serve_with(body, body.len(), false, Some(8)).await;
        let r = recording(Some(format!("{base}/p.mp4")));
        let d = dir("norange");
        assert!(
            recap()
                .download(&r, &d, &DownloadOptions::default(), |_| {})
                .await
                .is_err()
        );
        let got = recap()
            .download(&r, &d, &DownloadOptions::default(), |_| {})
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&got.written[0]).unwrap(),
            body.as_bytes(),
            "no corruption"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn partial_without_a_saved_validator_restarts() {
        let body = "0123456789abcdefghij";
        let base = serve_with(body, body.len(), true, None).await;
        let r = recording(Some(format!("{base}/p.mp4")));
        let d = dir("novalidator");
        let tmp = d.join(".canvas-sync/tmp");
        std::fs::create_dir_all(&tmp).unwrap();
        // A stale part from some other file, with no .meta.
        std::fs::write(tmp.join(format!("{}.part", r.id)), b"XXXXXXXX").unwrap();
        let got = recap()
            .download(&r, &d, &DownloadOptions::default(), |_| {})
            .await
            .unwrap();
        assert_eq!(
            std::fs::read(&got.written[0]).unwrap(),
            body.as_bytes(),
            "never spliced"
        );
        assert!(
            std::fs::read_dir(&tmp).unwrap().next().is_none(),
            "temp files cleaned up"
        );
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn content_range_parses() {
        assert_eq!(content_range("bytes 8-19/20"), Some((8, Some(20))));
        assert_eq!(content_range("bytes 8-19/*"), Some((8, None)));
        assert_eq!(content_range("items 1-2/3"), None);
    }

    #[tokio::test]
    async fn no_mp4_is_a_clear_error() {
        let d = dir("none");
        let err = recap()
            .download(&recording(None), &d, &DownloadOptions::default(), |_| {})
            .await
            .unwrap_err();
        assert!(err.to_string().contains("--streams"));
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[tokio::test]
    async fn remux_refuses_non_https() {
        let err = remux("file:///etc/hosts", Path::new("/tmp/x"))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not https"));
    }
}
