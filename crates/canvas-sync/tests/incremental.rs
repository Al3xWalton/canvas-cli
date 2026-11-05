//! A second sync with nothing changed must download nothing.

#![allow(clippy::unwrap_used)]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use canvas_auth::{Cookie, Session};
use canvas_core::Config;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A tiny Canvas stand-in: Files list hidden (403), one module linking one
/// file, which downloads as `hello`. Counts download hits.
async fn fake_canvas() -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(AtomicUsize::new(0));
    let (me, counter) = (base.clone(), hits.clone());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0; 8192];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).into_owned();
            let path = request.split_whitespace().nth(1).unwrap_or("").to_owned();
            let (status, body) = if path.starts_with("/api/v1/courses/1/files?") {
                ("403 Forbidden", r#"{"status":"unauthorized"}"#.to_owned())
            } else if path.starts_with("/api/v1/courses/1/modules") {
                ("200 OK", r#"[{"id":1,"name":"Week 1","position":1,"items":[{"id":9,"title":"Slides","type":"File","content_id":5},{"id":10,"title":"slides","type":"File","content_id":6}]}]"#.to_owned())
            } else if path.starts_with("/api/v1/courses/1/files/5") {
                (
                    "200 OK",
                    format!(
                        r#"{{"id":5,"display_name":"Slides.pdf","size":5,"updated_at":"2026-11-01T00:00:00Z","folder_id":2,"url":"{me}/dl/5"}}"#
                    ),
                )
            } else if path.starts_with("/api/v1/courses/1/files/6") {
                (
                    "200 OK",
                    format!(
                        r#"{{"id":6,"display_name":"slides.pdf","size":5,"updated_at":"2026-11-01T00:00:00Z","folder_id":2,"url":"{me}/dl/6"}}"#
                    ),
                )
            } else if path.starts_with("/dl/6") {
                ("200 OK", "world".to_owned())
            } else if path.starts_with("/api/v1/courses/1/folders") {
                (
                    "200 OK",
                    r#"[{"id":2,"full_name":"course files/Lectures"}]"#.to_owned(),
                )
            } else if path.starts_with("/dl/5") {
                counter.fetch_add(1, Ordering::SeqCst);
                ("200 OK", "hello".to_owned())
            } else {
                ("404 Not Found", String::new())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = socket.write_all(response.as_bytes()).await;
        }
    });
    (base, hits)
}

#[tokio::test]
async fn second_sync_downloads_nothing() {
    let (base, hits) = fake_canvas().await;
    let config = Config {
        canvas_url: base,
        ..Config::default()
    };
    let session = Session {
        captured_at: 0,
        cookies: vec![Cookie {
            name: "canvas_session".into(),
            value: "x".into(),
            domain: "127.0.0.1".into(),
            path: "/".into(),
            expires: None,
        }],
    };
    let client = canvas_api::Client::new(&config, &session).unwrap();
    let course: canvas_api::Course =
        serde_json::from_str(r#"{"id":1,"name":"Example Studies","course_code":"ABC1020"}"#)
            .unwrap();
    let root = std::env::temp_dir().join(format!("canvas-sync-it-{}", std::process::id()));

    let first = canvas_sync::sync_course(&client, &course, &root, |_| {})
        .await
        .unwrap();
    // `slides.pdf` differs only in case, which macOS treats as the same
    // name, so it gets its id appended.
    assert_eq!(
        first.downloaded,
        vec![
            "ABC1020/Lectures/Slides.pdf".to_owned(),
            "ABC1020/Lectures/slides (6).pdf".to_owned()
        ]
    );
    assert!(first.failed.is_empty());
    assert_eq!(
        std::fs::read(root.join("ABC1020/Lectures/Slides.pdf")).unwrap(),
        b"hello"
    );

    let second = canvas_sync::sync_course(&client, &course, &root, |_| {})
        .await
        .unwrap();
    assert!(second.downloaded.is_empty());
    assert_eq!(second.unchanged, 2);
    assert_eq!(hits.load(Ordering::SeqCst), 1);

    std::fs::remove_file(root.join("ABC1020/Lectures/Slides.pdf")).unwrap();
    let third = canvas_sync::sync_course(&client, &course, &root, |_| {})
        .await
        .unwrap();
    assert_eq!(
        third.downloaded.len(),
        1,
        "a deleted local copy is fetched again"
    );

    std::fs::remove_dir_all(&root).unwrap();
}
