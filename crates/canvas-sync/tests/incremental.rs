//! Sync and `get_file` against a fake Canvas: a second sync with nothing
//! changed downloads nothing, files linked from inside pages are found and
//! placed like a sync would, names stay with their files whatever order
//! syncs and gets run in, and `get_file` records what it fetches so a
//! later sync treats it as known.

#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use canvas_auth::{Cookie, Session};
use canvas_core::{Config, Error};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A tiny Canvas stand-in, recording each request's path.
///
/// Course 1 (`ABC1020`), Files list hidden (403):
/// - Module `Week 1` links files 5 (`Slides.pdf`) and 6 (`slides.pdf`) as
///   items, in folder `Lectures`, and the page `workshop`.
/// - The `workshop` page links file 7 (`Workshop.pdf`, in a hidden
///   folder), embeds file 8 as an image, and links file 9 of another
///   course.
/// - File 8 is locked; file 10 is another `Slides.pdf` in `Lectures`;
///   file 11 has a hostile name; file 17's download is refused; asking for
///   file 18 finds the session expired.
///
/// Course 3 (`XYZ`) shows its Files list, which holds file 12 at the top
/// level; its page `info` also links file 12.
///
/// Course 4 (`QRS`), Files list and folders hidden: module `Week 1` has
/// file 30 (`Notes.pdf`) as an item and the page `notes`, which links file
/// 25 (also `Notes.pdf`) and file 13, which Canvas no longer has.
///
/// Course 5 (`RPL`): page `deck` in module `Week 2` links file 78, which
/// was replaced: Canvas answers id 78 with file 79's record.
struct Fake {
    base: String,
    hits: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    /// How many requests asked for exactly `path` (without the query).
    fn count(&self, path: &str) -> usize {
        self.hits
            .lock()
            .unwrap()
            .iter()
            .filter(|p| *p == path)
            .count()
    }
}

async fn fake_canvas() -> Fake {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let hits = Arc::new(Mutex::new(Vec::new()));
    let (me, seen) = (base.clone(), hits.clone());
    tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let me = me.clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut buf = vec![0; 8192];
                let n = socket.read(&mut buf).await.unwrap();
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                let target = request.split_whitespace().nth(1).unwrap_or("").to_owned();
                let path = target.split('?').next().unwrap_or("").to_owned();
                seen.lock().unwrap().push(path.clone());
                let (status, body) = route(&me, &path);
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    Fake { base, hits }
}

/// The fake's answer to one request path.
fn route(me: &str, path: &str) -> (&'static str, String) {
    let file = |course: u64, id: u64, name: &str, size: u64, folder: u64| {
        let _ = course;
        format!(
            r#"{{"id":{id},"display_name":"{name}","size":{size},"updated_at":"2026-11-01T00:00:00Z","folder_id":{folder},"url":"{me}/dl/{id}"}}"#
        )
    };
    let ok = |body: String| ("200 OK", body);
    match path {
        "/api/v1/courses/1/files" | "/api/v1/courses/4/files" | "/api/v1/courses/5/files" => {
            ("403 Forbidden", r#"{"status":"unauthorized"}"#.to_owned())
        }
        "/api/v1/courses/1/modules" => ok(r#"[{"id":1,"name":"Week 1","position":1,"items":[{"id":9,"title":"Slides","type":"File","content_id":5},{"id":10,"title":"slides","type":"File","content_id":6},{"id":11,"title":"Workshop","type":"Page","page_url":"workshop"}]}]"#.to_owned()),
        "/api/v1/courses/1/pages/workshop" => ok(format!(
            r#"{{"url":"workshop","title":"Workshop","body":"<p><a href=\"{me}/courses/1/files/7?wrap=1\" title=\"Workshop.pdf\">Workshop slides</a></p><p><img src=\"{me}/courses/1/files/8/preview\" alt=\"chart\"></p><p><a href=\"{me}/courses/2/files/9?wrap=1\">Other course</a></p>"}}"#
        )),
        "/api/v1/courses/1/folders" => ok(r#"[{"id":2,"full_name":"course files/Lectures"}]"#.to_owned()),
        "/api/v1/courses/1/files/5" => ok(file(1, 5, "Slides.pdf", 5, 2)),
        "/api/v1/courses/1/files/6" => ok(file(1, 6, "slides.pdf", 5, 2)),
        "/api/v1/courses/1/files/7" => ok(file(1, 7, "Workshop.pdf", 4, 99)),
        "/api/v1/courses/1/files/8" => ok(r#"{"id":8,"display_name":"Answers.pdf","size":1,"locked_for_user":true,"lock_explanation":"This file is locked until 1 Dec.","url":""}"#.to_owned()),
        "/api/v1/courses/1/files/10" => ok(file(1, 10, "Slides.pdf", 3, 2)),
        "/api/v1/courses/1/files/11" => ok(file(1, 11, "../../escape.pdf", 3, 99)),
        "/api/v1/courses/1/files/17" => ok(file(1, 17, "Refused.pdf", 1, 2)),
        "/api/v1/courses/1/files/18" => (
            "401 Unauthorized",
            r#"{"status":"unauthenticated","errors":[{"message":"user authorization required"}]}"#.to_owned(),
        ),
        "/api/v1/courses/3/files" => ok(format!("[{}]", file(3, 12, "Handbook.pdf", 2, 30))),
        "/api/v1/courses/3/files/12" => ok(file(3, 12, "Handbook.pdf", 2, 30)),
        "/api/v1/courses/3/folders" => ok(r#"[{"id":30,"full_name":"course files"}]"#.to_owned()),
        "/api/v1/courses/3/modules" => ok(r#"[{"id":1,"name":"Week 1","position":1,"items":[{"id":1,"title":"Info","type":"Page","page_url":"info"}]}]"#.to_owned()),
        "/api/v1/courses/3/pages/info" => ok(format!(
            r#"{{"url":"info","title":"Info","body":"<a href=\"{me}/courses/3/files/12\">Handbook</a>"}}"#
        )),
        "/api/v1/courses/4/folders" => ok("[]".to_owned()),
        "/api/v1/courses/4/modules" => ok(r#"[{"id":1,"name":"Week 1","position":1,"items":[{"id":1,"title":"Notes","type":"File","content_id":30},{"id":2,"title":"Notes page","type":"Page","page_url":"notes"}]}]"#.to_owned()),
        "/api/v1/courses/4/pages/notes" => ok(format!(
            r#"{{"url":"notes","title":"Notes","body":"<a href=\"{me}/courses/4/files/25?wrap=1\">Notes</a> <a href=\"/courses/4/files/13\">Old</a>"}}"#
        )),
        "/api/v1/courses/4/files/30" => ok(file(4, 30, "Notes.pdf", 3, 99)),
        "/api/v1/courses/4/files/25" => ok(file(4, 25, "Notes.pdf", 3, 99)),
        "/api/v1/courses/5/folders" => ok("[]".to_owned()),
        "/api/v1/courses/5/modules" => ok(r#"[{"id":1,"name":"Week 2","position":1,"items":[{"id":1,"title":"Deck","type":"Page","page_url":"deck"}]}]"#.to_owned()),
        "/api/v1/courses/5/pages/deck" => ok(format!(
            r#"{{"url":"deck","title":"Deck","body":"<a href=\"{me}/courses/5/files/78?wrap=1\">Deck</a>"}}"#
        )),
        "/api/v1/courses/5/files/78" | "/api/v1/courses/5/files/79" => {
            ok(file(5, 79, "Deck.pdf", 4, 99))
        }
        "/dl/79" => ok("deck".to_owned()),
        "/dl/5" => ok("hello".to_owned()),
        "/dl/6" => ok("world".to_owned()),
        "/dl/7" => ok("work".to_owned()),
        "/dl/10" => ok("abc".to_owned()),
        "/dl/11" => ok("esc".to_owned()),
        "/dl/12" => ok("hb".to_owned()),
        "/dl/17" => ("403 Forbidden", String::new()),
        "/dl/25" => ok("p25".to_owned()),
        "/dl/30" => ok("m30".to_owned()),
        _ => ("404 Not Found", String::new()),
    }
}

fn client(base: &str) -> canvas_api::Client {
    let config = Config {
        canvas_url: base.to_owned(),
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
    canvas_api::Client::new(&config, &session).unwrap()
}

fn course(id: u64, code: &str) -> canvas_api::Course {
    serde_json::from_value(serde_json::json!({"id": id, "name": code, "course_code": code}))
        .unwrap()
}

fn abc() -> canvas_api::Course {
    course(1, "ABC1020")
}

fn qrs() -> canvas_api::Course {
    course(4, "QRS")
}

/// A fresh, empty download root for one test, and its canonical form.
fn root(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("canvas-sync-it-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let canonical = root.canonicalize().unwrap();
    (root, canonical)
}

/// The course's manifest, as JSON.
fn manifest(root: &Path, course: u64) -> serde_json::Value {
    let text = std::fs::read_to_string(root.join(format!(".canvas-sync/{course}.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

fn read(root: &Path, rel: &str) -> String {
    String::from_utf8(std::fs::read(root.join(rel)).unwrap()).unwrap()
}

async fn sync(
    client: &canvas_api::Client,
    course: &canvas_api::Course,
    root: &Path,
    linked: bool,
) -> canvas_sync::SyncReport {
    canvas_sync::sync_course(client, course, root, linked, |_| {})
        .await
        .unwrap()
}

#[tokio::test]
async fn second_sync_downloads_nothing() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("second");

    let first = sync(&client, &abc(), &root, false).await;
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
    assert_eq!(read(&root, "ABC1020/Lectures/Slides.pdf"), "hello");
    assert_eq!(
        fake.count("/api/v1/courses/1/pages/workshop"),
        0,
        "pages unread without linked"
    );

    let second = sync(&client, &abc(), &root, false).await;
    assert!(second.downloaded.is_empty());
    assert_eq!(second.unchanged, 2);
    assert_eq!(fake.count("/dl/5"), 1);

    std::fs::remove_file(root.join("ABC1020/Lectures/Slides.pdf")).unwrap();
    let third = sync(&client, &abc(), &root, false).await;
    assert_eq!(
        third.downloaded.len(),
        1,
        "a deleted local copy is fetched again"
    );

    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn listing_adds_page_links_and_cached_listing_skips_known_files() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let list = canvas_sync::list_files(&client, &abc()).await.unwrap();
    let found: Vec<(u64, canvas_sync::Source, Option<&str>)> = list
        .files
        .iter()
        .map(|l| (l.file.id, l.source, l.page.as_deref()))
        .collect();
    use canvas_sync::Source::{Module, Page};
    // File 8 is only an embedded image and file 9 is another course's.
    assert_eq!(
        found,
        vec![
            (5, Module, None),
            (6, Module, None),
            (7, Page, Some("workshop"))
        ]
    );
    assert!(list.failed.is_empty() && list.unavailable.is_empty() && list.warnings.is_empty());
    let json = serde_json::to_value(&list.files[2]).unwrap();
    assert_eq!(
        (json["source"].clone(), json["page"].clone()),
        ("page".into(), "workshop".into())
    );

    // The digest's cached listing reads no pages and looks up no known file.
    let known = list
        .files
        .iter()
        .map(|l| (l.file.id, l.file.clone()))
        .collect();
    let lookups = fake.count("/api/v1/courses/1/files/5");
    let (cached, _) = canvas_sync::list_files_cached(&client, &abc(), &known)
        .await
        .unwrap();
    assert_eq!(
        fake.count("/api/v1/courses/1/files/5"),
        lookups,
        "cached looks up nothing"
    );
    assert_eq!(
        fake.count("/api/v1/courses/1/pages/workshop"),
        1,
        "only list_files read the page"
    );
    let module_files: Vec<_> = list
        .files
        .iter()
        .filter(|l| l.source != Page)
        .map(|l| l.file.clone())
        .collect();
    assert_eq!(cached, module_files);
}

#[tokio::test]
async fn stale_page_links_are_unavailable_not_failures() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let list = canvas_sync::list_files(&client, &qrs()).await.unwrap();
    assert!(list.failed.is_empty(), "{:?}", list.failed);
    let gone: Vec<u64> = list.unavailable.iter().map(|f| f.file_id).collect();
    assert_eq!(gone, vec![13]);
    let (root, _) = root("stale");
    let report = sync(&client, &qrs(), &root, true).await;
    assert!(report.failed.is_empty(), "{:?}", report.failed);
    assert_eq!(report.unavailable.len(), 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn get_then_sync_treats_the_file_as_known() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, canonical) = root("get-sync");

    // File 7's folder is hidden, so it goes under the module of the page
    // linking it, as a linked sync would put it.
    let saved = canvas_sync::get_file(&client, &abc(), &root, 7)
        .await
        .unwrap();
    assert_eq!(saved.path, canonical.join("ABC1020/Week 1/Workshop.pdf"));
    assert!(saved.downloaded);
    assert_eq!(
        (saved.id, saved.name.as_str(), saved.size),
        (7, "Workshop.pdf", 4)
    );
    assert_eq!(std::fs::read(&saved.path).unwrap(), b"work");
    let entry = &manifest(&root, 1)["files"]["7"];
    assert_eq!(entry["path"], "ABC1020/Week 1/Workshop.pdf");
    assert_eq!(entry["linked"], true);
    let leftovers = std::fs::read_dir(canonical.join(".canvas-sync/tmp/1"))
        .unwrap()
        .count();
    assert_eq!(leftovers, 0, "the temp file was renamed into place");

    let report = sync(&client, &abc(), &root, true).await;
    assert!(
        !report.downloaded.iter().any(|p| p.contains("Workshop")),
        "{:?}",
        report.downloaded
    );
    assert_eq!(report.unchanged, 1, "file 7 is known");
    assert_eq!(report.downloaded.len(), 2, "files 5 and 6 are new");
    assert_eq!(fake.count("/dl/7"), 1);

    let again = canvas_sync::get_file(&client, &abc(), &root, 7)
        .await
        .unwrap();
    assert!(!again.downloaded, "already up to date");
    assert_eq!(fake.count("/dl/7"), 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn linked_sync_puts_page_files_where_get_would() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("linked");
    let plain = sync(&client, &abc(), &root, false).await;
    assert!(!plain.downloaded.iter().any(|p| p.contains("Workshop")));
    let linked = sync(&client, &abc(), &root, true).await;
    assert_eq!(
        linked.downloaded,
        vec!["ABC1020/Week 1/Workshop.pdf".to_owned()]
    );
    assert_eq!(linked.unchanged, 2, "files 5 and 6 did not move");
    let saved = canvas_sync::get_file(&client, &abc(), &root, 7)
        .await
        .unwrap();
    assert!(!saved.downloaded, "get finds sync's copy");
    assert_eq!(fake.count("/dl/7"), 1);
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn same_named_files_keep_their_names_across_plain_and_linked_syncs() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("clash-sync");
    // Module file 30 and page-only file 25 are both `Week 1/Notes.pdf`.
    let plain = sync(&client, &qrs(), &root, false).await;
    assert_eq!(plain.downloaded, vec!["QRS/Week 1/Notes.pdf".to_owned()]);
    let linked = sync(&client, &qrs(), &root, true).await;
    assert_eq!(
        linked.downloaded,
        vec!["QRS/Week 1/Notes (25).pdf".to_owned()],
        "the page-only file gives way"
    );
    assert_eq!(linked.unchanged, 1);
    for linked in [false, true, false] {
        let again = sync(&client, &qrs(), &root, linked).await;
        assert!(
            again.downloaded.is_empty(),
            "{linked}: {:?}",
            again.downloaded
        );
    }
    assert_eq!(read(&root, "QRS/Week 1/Notes.pdf"), "m30");
    assert_eq!(read(&root, "QRS/Week 1/Notes (25).pdf"), "p25");
    assert_eq!((fake.count("/dl/30"), fake.count("/dl/25")), (1, 1));
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn a_fresh_linked_sync_gives_the_module_file_the_plain_name() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("clash-fresh");
    // Page-only file 25 has the lower id, but module file 30 chooses first.
    let linked = sync(&client, &qrs(), &root, true).await;
    assert_eq!(linked.downloaded.len(), 2, "{:?}", linked.downloaded);
    assert_eq!(read(&root, "QRS/Week 1/Notes.pdf"), "m30");
    assert_eq!(read(&root, "QRS/Week 1/Notes (25).pdf"), "p25");
    let plain = sync(&client, &qrs(), &root, false).await;
    assert!(plain.downloaded.is_empty(), "{:?}", plain.downloaded);
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn a_plain_sync_never_overwrites_a_fetched_page_file() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("clash-get");
    // Fetched first, the page-only file takes the plain name...
    let saved = canvas_sync::get_file(&client, &qrs(), &root, 25)
        .await
        .unwrap();
    assert!(saved.path.ends_with("QRS/Week 1/Notes.pdf"), "{saved:?}");
    // ...so the module file, which a plain sync does see, gives way.
    let plain = sync(&client, &qrs(), &root, false).await;
    assert_eq!(
        plain.downloaded,
        vec!["QRS/Week 1/Notes (30).pdf".to_owned()]
    );
    for linked in [true, false] {
        let again = sync(&client, &qrs(), &root, linked).await;
        assert!(
            again.downloaded.is_empty(),
            "{linked}: {:?}",
            again.downloaded
        );
    }
    assert_eq!(read(&root, "QRS/Week 1/Notes.pdf"), "p25");
    assert_eq!(read(&root, "QRS/Week 1/Notes (30).pdf"), "m30");
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn a_replaced_file_takes_over_its_old_name() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("replaced");
    // File 99 once had `Notes.pdf` but is gone from Canvas: the lecturer
    // uploaded file 30 to replace it. Sync and get must agree on the name.
    std::fs::create_dir_all(root.join(".canvas-sync")).unwrap();
    std::fs::create_dir_all(root.join("QRS/Week 1")).unwrap();
    std::fs::write(root.join("QRS/Week 1/Notes.pdf"), "old").unwrap();
    std::fs::write(
        root.join(".canvas-sync/4.json"),
        r#"{"files":{"99":{"path":"QRS/Week 1/Notes.pdf","size":3,"updated_at":null}}}"#,
    )
    .unwrap();
    let saved = canvas_sync::get_file(&client, &qrs(), &root, 30)
        .await
        .unwrap();
    assert!(saved.path.ends_with("QRS/Week 1/Notes.pdf"), "{saved:?}");
    let plain = sync(&client, &qrs(), &root, false).await;
    assert!(plain.downloaded.is_empty(), "{:?}", plain.downloaded);
    assert_eq!(read(&root, "QRS/Week 1/Notes.pdf"), "m30");
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn a_linked_sync_never_overwrites_a_file_got_from_elsewhere() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("got-elsewhere");
    // File 10 (`Lectures/Slides.pdf`) is in no module or page: say a link
    // in an announcement. Files 5 and 6 want the same name.
    canvas_sync::get_file(&client, &abc(), &root, 10)
        .await
        .unwrap();
    let linked = sync(&client, &abc(), &root, true).await;
    assert!(
        linked
            .downloaded
            .contains(&"ABC1020/Lectures/Slides (5).pdf".to_owned()),
        "{:?}",
        linked.downloaded
    );
    assert_eq!(
        read(&root, "ABC1020/Lectures/Slides.pdf"),
        "abc",
        "file 10's copy"
    );
    for linked in [false, true] {
        let again = sync(&client, &abc(), &root, linked).await;
        assert!(
            again.downloaded.is_empty(),
            "{linked}: {:?}",
            again.downloaded
        );
    }
    let again = canvas_sync::get_file(&client, &abc(), &root, 10)
        .await
        .unwrap();
    assert!(!again.downloaded);
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn a_replaced_page_file_takes_over_its_old_name() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("replaced-page");
    // File 77 was linked from the workshop page and fetched; Canvas no
    // longer has it (file 7 replaced it), but its entry and copy remain.
    std::fs::create_dir_all(root.join(".canvas-sync")).unwrap();
    std::fs::create_dir_all(root.join("ABC1020/Week 1")).unwrap();
    std::fs::write(root.join("ABC1020/Week 1/Workshop.pdf"), "old!").unwrap();
    std::fs::write(
        root.join(".canvas-sync/1.json"),
        r#"{"files":{"77":{"path":"ABC1020/Week 1/Workshop.pdf","size":4,"updated_at":null,"linked":true}}}"#,
    )
    .unwrap();
    let linked = sync(&client, &abc(), &root, true).await;
    assert!(
        linked
            .downloaded
            .contains(&"ABC1020/Week 1/Workshop.pdf".to_owned()),
        "{:?}",
        linked.downloaded
    );
    assert_eq!(read(&root, "ABC1020/Week 1/Workshop.pdf"), "work");
    assert!(
        manifest(&root, 1)["files"]["77"].is_null(),
        "the old entry is gone"
    );
    let saved = canvas_sync::get_file(&client, &abc(), &root, 7)
        .await
        .unwrap();
    assert!(!saved.downloaded, "get agrees with sync");
    assert!(saved.path.ends_with("ABC1020/Week 1/Workshop.pdf"));
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn a_file_taken_out_of_a_module_keeps_its_copy() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("taken-out");
    // File 10 was synced from a module; the lecturer took it out of the
    // module but Canvas still has it. File 5 wants its name.
    std::fs::create_dir_all(root.join(".canvas-sync")).unwrap();
    std::fs::create_dir_all(root.join("ABC1020/Lectures")).unwrap();
    std::fs::write(root.join("ABC1020/Lectures/Slides.pdf"), "keep").unwrap();
    std::fs::write(
        root.join(".canvas-sync/1.json"),
        r#"{"files":{"10":{"path":"ABC1020/Lectures/Slides.pdf","size":4,"updated_at":null}}}"#,
    )
    .unwrap();
    let first = sync(&client, &abc(), &root, false).await;
    assert!(
        first
            .downloaded
            .contains(&"ABC1020/Lectures/Slides (5).pdf".to_owned()),
        "{:?}",
        first.downloaded
    );
    assert_eq!(read(&root, "ABC1020/Lectures/Slides.pdf"), "keep");
    let asked = fake.count("/api/v1/courses/1/files/10");
    assert_eq!(asked, 1, "Canvas was asked about the holder once");
    let again = sync(&client, &abc(), &root, false).await;
    assert!(again.downloaded.is_empty());
    assert_eq!(
        fake.count("/api/v1/courses/1/files/10"),
        asked,
        "settled names cost no lookups"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn a_replaced_files_old_id_saves_the_replacement_once() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("old-id");
    let rpl = course(5, "RPL");
    let list = canvas_sync::list_files(&client, &rpl).await.unwrap();
    let found: Vec<(u64, Option<&str>)> = list
        .files
        .iter()
        .map(|l| (l.file.id, l.page.as_deref()))
        .collect();
    assert_eq!(found, vec![(79, Some("deck"))], "filed under its own id");
    let saved = canvas_sync::get_file(&client, &rpl, &root, 78)
        .await
        .unwrap();
    assert_eq!(saved.id, 79);
    assert!(saved.path.ends_with("RPL/Week 2/Deck.pdf"), "{saved:?}");
    let report = sync(&client, &rpl, &root, true).await;
    assert_eq!(
        (report.downloaded.len(), report.unchanged),
        (0, 1),
        "{report:?}"
    );
    let files = &manifest(&root, 5)["files"];
    assert!(files["79"].is_object() && files["78"].is_null(), "{files}");
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn an_old_id_in_the_manifest_gives_way_to_its_replacement() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("old-entry");
    // An earlier version saved the deck under the old id 78.
    std::fs::create_dir_all(root.join(".canvas-sync")).unwrap();
    std::fs::create_dir_all(root.join("RPL/Week 2")).unwrap();
    std::fs::write(root.join("RPL/Week 2/Deck.pdf"), "old!").unwrap();
    std::fs::write(
        root.join(".canvas-sync/5.json"),
        r#"{"files":{"78":{"path":"RPL/Week 2/Deck.pdf","size":4,"updated_at":null,"linked":true}}}"#,
    )
    .unwrap();
    let report = sync(&client, &course(5, "RPL"), &root, true).await;
    assert_eq!(report.downloaded, vec!["RPL/Week 2/Deck.pdf".to_owned()]);
    assert_eq!(read(&root, "RPL/Week 2/Deck.pdf"), "deck");
    assert!(manifest(&root, 5)["files"]["78"].is_null());
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn an_unreadable_manifest_stops_the_sync_instead_of_resetting() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("unreadable");
    // A manifest that cannot be read at all (here a folder) is an error,
    // not a reason to download everything again.
    std::fs::create_dir_all(root.join(".canvas-sync/1.json")).unwrap();
    let err = canvas_sync::sync_course(&client, &abc(), &root, false, |_| {})
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Io { .. }), "{err}");
    assert!(root.join(".canvas-sync/1.json").is_dir());
    assert_eq!(fake.count("/dl/5"), 0);
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn interrupted_downloads_are_cleared() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("leftovers");
    let tmp = root.join(".canvas-sync/tmp/1");
    std::fs::create_dir_all(&tmp).unwrap();
    std::fs::write(tmp.join("5.123.0.part"), "half a download").unwrap();
    // Earlier versions kept temp files at the top: a day-old one goes, a
    // fresh one (another version may be writing it) stays.
    let top = root.join(".canvas-sync/tmp");
    std::fs::write(top.join("6.part"), "old").unwrap();
    let two_days = std::time::Duration::from_secs(2 * 24 * 60 * 60);
    std::fs::File::options()
        .write(true)
        .open(top.join("6.part"))
        .unwrap()
        .set_modified(std::time::SystemTime::now() - two_days)
        .unwrap();
    std::fs::write(top.join("7.part"), "new").unwrap();
    sync(&client, &abc(), &root, false).await;
    assert_eq!(std::fs::read_dir(&tmp).unwrap().count(), 0);
    assert!(!top.join("6.part").exists() && top.join("7.part").exists());
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn concurrent_gets_keep_both_files_and_entries() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("concurrent");
    let course = qrs();
    let (a, b) = tokio::join!(
        canvas_sync::get_file(&client, &course, &root, 30),
        canvas_sync::get_file(&client, &course, &root, 25)
    );
    let (a, b) = (a.unwrap(), b.unwrap());
    assert_ne!(a.path, b.path, "both cannot have Notes.pdf");
    assert_eq!(std::fs::read(&a.path).unwrap(), b"m30");
    assert_eq!(std::fs::read(&b.path).unwrap(), b"p25");
    let files = &manifest(&root, 4)["files"];
    assert!(
        files["30"].is_object() && files["25"].is_object(),
        "{files}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn get_never_takes_another_files_path() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, canonical) = root("taken");
    sync(&client, &abc(), &root, false).await;
    // File 10 is also `Slides.pdf` in `Lectures`, which file 5 holds.
    let saved = canvas_sync::get_file(&client, &abc(), &root, 10)
        .await
        .unwrap();
    assert!(
        saved.path.ends_with("ABC1020/Lectures/Slides (10).pdf"),
        "{saved:?}"
    );
    assert_eq!(
        std::fs::read(canonical.join("ABC1020/Lectures/Slides.pdf")).unwrap(),
        b"hello",
        "file 5's copy is untouched"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn files_list_files_are_not_moved_by_page_links() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("listed");
    let other = course(3, "XYZ");
    // File 12 is in the visible Files list at the top level: a sync puts it
    // in the course folder, page link or not, so get must too.
    let saved = canvas_sync::get_file(&client, &other, &root, 12)
        .await
        .unwrap();
    assert!(saved.path.ends_with("XYZ/Handbook.pdf"), "{saved:?}");
    let report = sync(&client, &other, &root, true).await;
    assert_eq!(
        (report.downloaded.len(), report.unchanged),
        (0, 1),
        "{report:?}"
    );
    let list = canvas_sync::list_files(&client, &other).await.unwrap();
    assert_eq!(list.files[0].source, canvas_sync::Source::Files);
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn hostile_names_stay_inside_the_root() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, canonical) = root("hostile");
    let saved = canvas_sync::get_file(&client, &abc(), &root, 11)
        .await
        .unwrap();
    assert!(saved.path.starts_with(&canonical), "{saved:?}");
    assert_eq!(
        saved.path.parent().unwrap(),
        canonical.join("ABC1020"),
        "{saved:?}"
    );
    assert!(!canonical.parent().unwrap().join("escape.pdf").exists());
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn locked_refused_missing_and_expired_say_why() {
    let fake = fake_canvas().await;
    let client = client(&fake.base);
    let (root, _) = root("errors");
    let err = canvas_sync::get_file(&client, &abc(), &root, 8)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Forbidden(_)), "{err}");
    let text = err.to_string();
    assert!(
        text.contains("\"Answers.pdf\"") && text.contains("locked until 1 Dec"),
        "{text}"
    );
    assert_eq!(fake.count("/dl/8"), 0);

    let err = canvas_sync::get_file(&client, &abc(), &root, 17)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::Forbidden(_)), "{err}");
    assert!(err.to_string().contains("refused to download"), "{err}");

    let err = canvas_sync::get_file(&client, &abc(), &root, 404)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::NotFound(_)), "{err}");
    assert!(err.to_string().contains("file 404 in ABC1020"), "{err}");

    let err = canvas_sync::get_file(&client, &abc(), &root, 18)
        .await
        .unwrap_err();
    assert!(matches!(err, Error::SessionExpired), "{err}");

    // A batch reports each file, and stops after an expired session,
    // keeping what it already saved.
    let (results, _) = canvas_sync::get_files(
        &client,
        &abc(),
        &root,
        &[7, 404],
        canvas_sync::GET_LOCK_WAIT,
    )
    .await
    .unwrap();
    assert!(results[0].is_ok() && results[1].is_err());
    let (results, _) = canvas_sync::get_files(
        &client,
        &abc(),
        &root,
        &[7, 18, 5],
        canvas_sync::GET_LOCK_WAIT,
    )
    .await
    .unwrap();
    assert_eq!(
        results.len(),
        2,
        "nothing is tried after the session expires"
    );
    assert!(results[0].is_ok());
    assert!(matches!(results[1], Err(Error::SessionExpired)));
    std::fs::remove_dir_all(&root).unwrap();
}
