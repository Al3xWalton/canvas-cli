//! Downloading recordings to disk.
//!
//! - **MP4 (default).** Panopto's composite MP4 is a signed CDN URL, fetched
//!   without any cookie, written to `<dir>/.canvas-sync/tmp/<id>.part`,
//!   checked against `Content-Length`, then renamed into place. No ffmpeg.
//! - **Separate streams (`--streams`).** Camera and screen feeds exist only
//!   as HLS, so `ffmpeg -c copy` remuxes each to MP4 without re-encoding.
//!   ffmpeg is restricted to https (`-protocol_whitelist`), so a hostile
//!   playlist cannot make it read local files.
//! - **Idempotent.** A file already present under its final name is
//!   skipped; names come from the recording date and title, sanitised.

use std::path::{Path, PathBuf};

use canvas_core::{Error, Result, sanitize};
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
                let part = tmp.join(format!("{}-{pid}.part", recording.id));
                match self.fetch_to(url, &part).await {
                    Ok(bytes) => {
                        std::fs::rename(&part, &dest).map_err(io(&dest))?;
                        out.bytes += bytes;
                        out.written.push(dest);
                    }
                    Err(e) => {
                        let _ = std::fs::remove_file(&part);
                        return Err(e);
                    }
                }
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

    /// Fetches a signed CDN URL (no cookie) into `dest`, checking length.
    async fn fetch_to(&self, url: &str, dest: &Path) -> Result<u64> {
        let parsed = url::Url::parse(url).map_err(|e| Error::Http(e.to_string()))?;
        if parsed.scheme() != "https" && !cfg!(test) {
            return Err(Error::Http("ReCap MP4 URL is not https".into()));
        }
        let mut response = self
            .http
            .get(parsed)
            .send()
            .await
            .map_err(|e| Error::Http(e.to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Error::Http(format!("ReCap MP4 download returned {status}")));
        }
        let expected = response.content_length();
        let io = |source| Error::Io {
            path: dest.to_owned(),
            source,
        };
        let mut file = std::fs::File::create(dest).map_err(io)?;
        let mut written = 0u64;
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|e| Error::Http(e.to_string()))?
        {
            std::io::Write::write_all(&mut file, &chunk).map_err(io)?;
            written += chunk.len() as u64;
        }
        file.sync_all().map_err(io)?;
        if let Some(expected) = expected.filter(|e| *e != written) {
            return Err(Error::Http(format!(
                "ReCap MP4 was {written} bytes, expected {expected}; it may have been cut off"
            )));
        }
        Ok(written)
    }
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
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut buf = vec![0; 4096];
                let _ = socket.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {length}\r\nConnection: close\r\n\r\n"
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = socket.write_all(body.as_bytes()).await;
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
