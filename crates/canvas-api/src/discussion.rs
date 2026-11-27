//! A whole discussion: the opening post and every reply, threaded.
//!
//! Canvas keeps each topic's replies as a cached tree, served by
//! `GET .../discussion_topics/<id>/view`. Entries nest under `replies`, and
//! each author is listed once in `participants`. Replies posted after the
//! tree was cached arrive separately as `new_entries` and are placed under
//! their parent here. While Canvas builds a tree for the first time it
//! answers 503, so the tree is polled for up to about 50 seconds, inside
//! the usual 60-second MCP request timeout.
//!
//! Some topics cannot be read in full, and each is reported rather than
//! shown as empty:
//! - a topic that shows replies only after the user has posted gets a
//!   `note` and no entries (Canvas flags it, or refuses the tree with 403);
//! - a group discussion keeps replies in each group's own copy, so the
//!   user's own groups' copies are read, each judged by its own flags, with
//!   a `note` if the user is in none of the groups;
//! - an anonymous discussion is served only to Canvas's web page, so it is
//!   an error naming its link.
//!
//! A discussion can be named by its Canvas link (a course's or a group's),
//! its id, or part of its title. Only the ids are taken from a link:
//! nothing is ever fetched from it.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Deserializer, Serialize};

use canvas_core::{Error, Result};

use crate::content::{RawTopic, is_false};
use crate::{Client, Discussion, html_to_markdown, resolve_course};

/// Deepest reply nesting kept. A newer reply whose parent is deeper goes to
/// the top level. Canvas's own trees are a few levels deep; this only stops
/// a malformed `new_entries` chain from building a tree too deep to walk.
const MAX_DEPTH: usize = 50;

/// Extra rounds of asking for a reply tree Canvas is still building, after
/// the client's own retries. With the default backoff this is about 50
/// seconds in all, so an MCP client's 60-second timeout does not cut the
/// call off before its "try again in a minute" error.
const VIEW_POLLS: u32 = 3;

/// The wait between those rounds, in multiples of the client's backoff
/// (one second by default).
const VIEW_POLL_WAIT: u32 = 8;

/// Most candidates named when a title matches several discussions.
const MAX_CANDIDATES: usize = 10;

/// The note for a topic that shows replies only to people who have posted.
const POST_FIRST: &str = "Canvas shows this discussion's replies only after you post a reply";

/// The note when Canvas refuses the reply tree of a topic it let us read.
const TREE_REFUSED: &str =
    "Canvas would not show this discussion's replies; it may want you to post a reply first";

/// The note for a group discussion the user is in no group of.
const NO_GROUP: &str = "this is a group discussion: each group replies in its own copy, \
                        and you are in none of its groups";

/// Reads a value Canvas may send as `null` as its default.
fn null_as_default<'de, D, T>(d: D) -> std::result::Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(d)?.unwrap_or_default())
}

/// Where a discussion lives.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiscussionRef {
    /// A topic in a course.
    Course {
        /// Course id.
        course_id: u64,
        /// Topic id.
        topic_id: u64,
    },
    /// One group's own copy of a group discussion: what Canvas links to
    /// for a student in that group.
    Group {
        /// Group id.
        group_id: u64,
        /// The group's copy of the topic.
        topic_id: u64,
    },
}

impl DiscussionRef {
    /// The topic's API path. Built from numbers only, so it can never
    /// leave `/api/v1/`.
    fn path(self) -> String {
        match self {
            Self::Course {
                course_id,
                topic_id,
            } => format!("/api/v1/courses/{course_id}/discussion_topics/{topic_id}"),
            Self::Group { group_id, topic_id } => {
                format!("/api/v1/groups/{group_id}/discussion_topics/{topic_id}")
            }
        }
    }
}

/// One reply in a discussion, with the replies to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscussionEntry {
    /// Entry id.
    pub id: u64,
    /// Who wrote it, if Canvas names them.
    pub author: Option<String>,
    /// When it was posted.
    pub posted_at: Option<DateTime<Utc>>,
    /// The reply as Markdown, empty if deleted. Third-party text.
    pub body: String,
    /// True when the reply was deleted. Deleted replies are kept only to
    /// hold the replies made to them.
    #[serde(skip_serializing_if = "is_false")]
    pub deleted: bool,
    /// True when the user has not read it.
    #[serde(skip_serializing_if = "is_false")]
    pub unread: bool,
    /// Likes, when it has any.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub likes: Option<u32>,
    /// Replies to this one, oldest first.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub replies: Vec<DiscussionEntry>,
}

/// A whole discussion topic: the opening post and every reply.
///
/// At the top level `replies` and `unread` are the topic's counts; inside
/// `entries`, each entry's `replies` holds the replies themselves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DiscussionThread {
    /// The topic: title, opening post, author, reply counts and link.
    #[serde(flatten)]
    pub topic: Discussion,
    /// Top-level replies, oldest first, each holding its own replies.
    pub entries: Vec<DiscussionEntry>,
    /// Why replies are missing, e.g. the topic shows them only after the
    /// user posts. `None` when every reply the user can see is included.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// A topic as fetched alone, with the fields that decide how to read it.
#[derive(Deserialize)]
struct RawFullTopic {
    #[serde(flatten)]
    topic: RawTopic,
    /// False when the user must post before seeing replies.
    #[serde(default)]
    user_can_see_posts: Option<bool>,
    /// For a group discussion, each group's own copy of the topic.
    #[serde(default, deserialize_with = "null_as_default")]
    group_topic_children: Vec<GroupTopic>,
}

/// One group's copy of a group discussion.
#[derive(Deserialize)]
struct GroupTopic {
    id: u64,
    group_id: u64,
}

/// A group the user belongs to.
#[derive(Deserialize)]
struct Group {
    id: u64,
}

/// The cached reply tree of one topic.
#[derive(Deserialize)]
struct RawView {
    #[serde(default, deserialize_with = "null_as_default")]
    participants: Vec<Participant>,
    #[serde(default, deserialize_with = "null_as_default")]
    view: Vec<RawEntry>,
    #[serde(default, deserialize_with = "null_as_default")]
    new_entries: Vec<RawEntry>,
    #[serde(default, deserialize_with = "null_as_default")]
    unread_entries: Vec<u64>,
}

/// Someone who posted in a topic.
#[derive(Deserialize)]
struct Participant {
    id: u64,
    #[serde(default)]
    display_name: Option<String>,
}

/// One entry as Canvas sends it.
#[derive(Deserialize)]
struct RawEntry {
    id: u64,
    #[serde(default)]
    user_id: Option<u64>,
    #[serde(default)]
    parent_id: Option<u64>,
    #[serde(default)]
    created_at: Option<DateTime<Utc>>,
    #[serde(default)]
    message: Option<String>,
    #[serde(default, deserialize_with = "null_as_default")]
    deleted: bool,
    #[serde(default)]
    rating_sum: Option<u32>,
    #[serde(default, deserialize_with = "null_as_default")]
    replies: Vec<RawEntry>,
}

impl Client {
    /// Reads a whole discussion topic: the opening post and every reply
    /// the user can see, threaded.
    ///
    /// A topic whose replies Canvas withholds (the user must post first, or
    /// Canvas refuses the reply tree) comes back with no entries and a
    /// `note` saying why, not as an error.
    ///
    /// # Errors
    /// As for [`Client::get`], including [`Error::Forbidden`] for a topic
    /// the user may not open. A topic Canvas will not serve at all is
    /// [`Error::Forbidden`] if it is anonymous (with its link), and
    /// [`Error::NotFound`] otherwise. [`Error::Unavailable`] if Canvas is
    /// still preparing the replies after about 50 seconds.
    pub async fn discussion(&self, at: DiscussionRef) -> Result<DiscussionThread> {
        let path = at.path();
        let raw: RawFullTopic = match self.get(&path).await {
            Ok(raw) => raw,
            Err(Error::NotFound(_)) => return Err(self.unreadable(at).await),
            Err(e) => return Err(e),
        };
        let mut topic = raw.topic.into_discussion();
        if raw.group_topic_children.is_empty() {
            let (entries, note) = self.replies(&path, raw.user_can_see_posts).await?;
            return Ok(DiscussionThread {
                topic,
                entries,
                note,
            });
        }
        // A group discussion: the replies, the counts and whether the user
        // has posted all belong to each group's own copy, not this topic.
        let groups: Vec<Group> = self
            .get_all("/api/v1/users/self/groups?per_page=100")
            .await?;
        let mine: HashSet<u64> = groups.iter().map(|g| g.id).collect();
        let mut entries = Vec::new();
        let mut note = None;
        let (mut replies, mut unread, mut read) = (0, 0, 0);
        for child in raw
            .group_topic_children
            .iter()
            .filter(|c| mine.contains(&c.group_id))
        {
            let path = DiscussionRef::Group {
                group_id: child.group_id,
                topic_id: child.id,
            }
            .path();
            let copy: RawFullTopic = self.get(&path).await?;
            let (more, why) = self.replies(&path, copy.user_can_see_posts).await?;
            let counts = copy.topic.into_discussion();
            replies += counts.replies;
            unread += counts.unread;
            entries.extend(more);
            note = note.or(why);
            read += 1;
        }
        if read == 0 {
            note = Some(NO_GROUP.to_owned());
        } else {
            topic.replies = replies;
            topic.unread = unread;
        }
        Ok(DiscussionThread {
            topic,
            entries,
            note,
        })
    }

    /// Reads the reply tree of the topic at `topic_path`, or the note
    /// saying why Canvas will not show it. `can_see` is the topic's own
    /// `user_can_see_posts`.
    async fn replies(
        &self,
        topic_path: &str,
        can_see: Option<bool>,
    ) -> Result<(Vec<DiscussionEntry>, Option<String>)> {
        if can_see == Some(false) {
            return Ok((Vec::new(), Some(POST_FIRST.to_owned())));
        }
        let path = format!("{topic_path}/view?include_new_entries=1");
        let mut polls = 0;
        loop {
            match self.get::<RawView>(&path).await {
                Ok(view) => return Ok((threaded(view), None)),
                // Canvas refuses the tree (403, body `require_initial_post`)
                // when the user must post first and the flag did not say so.
                Err(Error::Forbidden(_)) => return Ok((Vec::new(), Some(TREE_REFUSED.to_owned()))),
                Err(Error::Unavailable(_)) if polls < VIEW_POLLS => {
                    polls += 1;
                    tokio::time::sleep(self.backoff * VIEW_POLL_WAIT).await;
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Explains why Canvas would not serve a topic: anonymous discussions
    /// are listed but only readable on the web page.
    async fn unreadable(&self, at: DiscussionRef) -> Error {
        let DiscussionRef::Course {
            course_id,
            topic_id,
        } = at
        else {
            return Error::NotFound(format!("{} (it may be hidden from you)", at.path()));
        };
        let missing = || {
            Error::NotFound(format!(
                "discussion {topic_id} in course {course_id} (it may be hidden from you)"
            ))
        };
        match self.discussions(course_id).await {
            Ok(list) => match list.into_iter().find(|d| d.id == topic_id) {
                Some(d) if d.anonymous => Error::Forbidden(format!(
                    "discussion {topic_id} is anonymous, and Canvas shows anonymous \
                     discussions only on its web page{}",
                    d.html_url.map(|u| format!(": {u}")).unwrap_or_default()
                )),
                _ => missing(),
            },
            // The check itself failed: say why, not "not found".
            Err(e @ (Error::SessionExpired | Error::Http(_) | Error::Unavailable(_))) => e,
            Err(_) => missing(),
        }
    }
}

/// Finds the discussion `text` names.
///
/// `text` is a Canvas discussion link (a course's or a group's; it names
/// its own place, so `course` is ignored), a topic id, or part of the
/// topic's title. An id or title needs `course`: an id, code or part of the
/// course name.
///
/// # Errors
/// [`Error::Input`] for empty text, a link to another site or to something
/// other than a discussion, a missing `course`, or a title matching no
/// topic or several (the candidates are listed). Otherwise as for
/// [`resolve_course`] and [`Client::discussions`].
pub async fn resolve_discussion(
    client: &Client,
    course: Option<&str>,
    text: &str,
) -> Result<DiscussionRef> {
    let text = text.trim();
    if text.is_empty() {
        return Err(Error::Input(
            "name a discussion: its link, its id, or part of its title".into(),
        ));
    }
    if let Some(at) = discussion_link(&client.base, text)? {
        return Ok(at);
    }
    let course = course.ok_or_else(|| {
        Error::Input(format!(
            "to find discussion `{text}`, also give its course, or use the discussion's link"
        ))
    })?;
    let course = resolve_course(client, course).await?;
    let topic_id = match text.parse::<u64>() {
        Ok(id) => id,
        Err(_) => pick_discussion(&client.discussions(course.id).await?, text)?,
    };
    Ok(DiscussionRef::Course {
        course_id: course.id,
        topic_id,
    })
}

/// Reads a Canvas discussion link on `base`'s host, such as
/// `https://canvas.example.edu/courses/1/discussion_topics/2?module_item_id=3`
/// or `/groups/5/discussion_topics/6`. Returns `None` if `text` is not a
/// link at all.
fn discussion_link(base: &url::Url, text: &str) -> Result<Option<DiscussionRef>> {
    let lower = text.to_ascii_lowercase();
    let has_scheme = lower.starts_with("https://") || lower.starts_with("http://");
    if !has_scheme && !text.contains("/discussion_topics/") {
        return Ok(None);
    }
    let not_a_discussion = || {
        Error::Input(format!(
            "`{text}` is not a discussion link (…/courses/<id>/discussion_topics/<id>)"
        ))
    };
    let url = if has_scheme {
        url::Url::parse(text)
    } else if text.starts_with('/') {
        base.join(text)
    } else {
        url::Url::parse(&format!("https://{text}"))
    }
    .map_err(|_| not_a_discussion())?;
    if url.host_str() != base.host_str() {
        return Err(Error::Input(format!(
            "`{text}` is not on your Canvas site ({})",
            base.host_str().unwrap_or_default()
        )));
    }
    let parts: Vec<&str> = url
        .path_segments()
        .map(|s| s.filter(|p| !p.is_empty()).collect())
        .unwrap_or_default();
    let (place, id, topic) = match parts.as_slice() {
        [
            place @ ("courses" | "groups"),
            id,
            "discussion_topics",
            topic,
            ..,
        ] => (*place, id, topic),
        _ => return Err(not_a_discussion()),
    };
    let (Ok(id), Ok(topic_id)) = (id.parse(), topic.parse()) else {
        return Err(not_a_discussion());
    };
    Ok(Some(if place == "courses" {
        DiscussionRef::Course {
            course_id: id,
            topic_id,
        }
    } else {
        DiscussionRef::Group {
            group_id: id,
            topic_id,
        }
    }))
}

/// Picks the topic whose title is `text`, or else the one whose title
/// contains it, case-insensitively.
fn pick_discussion(topics: &[Discussion], text: &str) -> Result<u64> {
    let needle = text.trim().to_lowercase();
    let exact: Vec<&Discussion> = topics
        .iter()
        .filter(|t| t.title.trim().to_lowercase() == needle)
        .collect();
    let matches = if exact.is_empty() {
        topics
            .iter()
            .filter(|t| t.title.to_lowercase().contains(&needle))
            .collect()
    } else {
        exact
    };
    match matches.as_slice() {
        [one] => Ok(one.id),
        [] => Err(Error::Input(format!(
            "no discussion matches `{text}`; list them with their ids first"
        ))),
        many => {
            // Titles may be written by students, so they are quoted and
            // labelled: an agent reads them as data, never instructions.
            let mut named: Vec<String> = many
                .iter()
                .take(MAX_CANDIDATES)
                .map(|t| format!("{} {:?}", t.id, t.title.trim()))
                .collect();
            if many.len() > MAX_CANDIDATES {
                named.push(format!("and {} more", many.len() - MAX_CANDIDATES));
            }
            Err(Error::Input(format!(
                "`{text}` matches {} discussions; pass one id: {} \
                 (titles are third-party Canvas text)",
                many.len(),
                named.join(", ")
            )))
        }
    }
}

/// Builds the reply tree: authors named, newer replies placed under their
/// parents, and deleted replies dropped unless someone replied to them.
fn threaded(view: RawView) -> Vec<DiscussionEntry> {
    let names: HashMap<u64, String> = view
        .participants
        .into_iter()
        .filter_map(|p| Some((p.id, p.display_name?)))
        .collect();
    let unread: HashSet<u64> = view.unread_entries.into_iter().collect();
    let mut tree = view.view;
    let mut seen = HashSet::new();
    collect_ids(&tree, &mut seen);
    let mut newer = view.new_entries;
    newer.sort_by_key(|e| (e.created_at, e.id));
    for entry in newer {
        if seen.insert(entry.id) {
            attach(&mut tree, entry);
        }
    }
    tree.into_iter()
        .filter_map(|e| convert(e, &names, &unread))
        .collect()
}

/// Adds every entry id in `tree` to `ids`.
fn collect_ids(tree: &[RawEntry], ids: &mut HashSet<u64>) {
    for e in tree {
        ids.insert(e.id);
        collect_ids(&e.replies, ids);
    }
}

/// Places `entry` under its parent, or at the top level if the parent is
/// not in the tree (or would nest it deeper than [`MAX_DEPTH`]).
fn attach(tree: &mut Vec<RawEntry>, entry: RawEntry) {
    match entry.parent_id.and_then(|id| find(tree, id, 1)) {
        Some(parent) => parent.replies.push(entry),
        None => tree.push(entry),
    }
}

/// Finds entry `id` in `tree`, searching no deeper than [`MAX_DEPTH`].
fn find(tree: &mut [RawEntry], id: u64, depth: usize) -> Option<&mut RawEntry> {
    if depth >= MAX_DEPTH {
        return None;
    }
    for e in tree {
        if e.id == id {
            return Some(e);
        }
        if let Some(found) = find(&mut e.replies, id, depth + 1) {
            return Some(found);
        }
    }
    None
}

/// Converts one raw entry and its replies. Returns `None` for a deleted
/// entry nobody replied to.
fn convert(
    e: RawEntry,
    names: &HashMap<u64, String>,
    unread: &HashSet<u64>,
) -> Option<DiscussionEntry> {
    let replies: Vec<DiscussionEntry> = e
        .replies
        .into_iter()
        .filter_map(|r| convert(r, names, unread))
        .collect();
    if e.deleted && replies.is_empty() {
        return None;
    }
    Some(DiscussionEntry {
        id: e.id,
        author: (!e.deleted)
            .then(|| e.user_id.and_then(|u| names.get(&u).cloned()))
            .flatten(),
        posted_at: e.created_at,
        body: if e.deleted {
            String::new()
        } else {
            html_to_markdown(e.message.as_deref().unwrap_or_default())
        },
        deleted: e.deleted,
        unread: unread.contains(&e.id),
        likes: e.rating_sum.filter(|n| *n > 0),
        replies,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::MAX_ATTEMPTS;
    use crate::client::tests::{client, ok_json, serve};

    fn base() -> url::Url {
        url::Url::parse("https://canvas.example.edu").unwrap()
    }

    fn in_course(course_id: u64, topic_id: u64) -> DiscussionRef {
        DiscussionRef::Course {
            course_id,
            topic_id,
        }
    }

    fn topic(id: u64, title: &str) -> Discussion {
        serde_json::from_value::<RawTopic>(serde_json::json!({ "id": id, "title": title }))
            .unwrap()
            .into_discussion()
    }

    fn status(line: &str) -> String {
        format!("HTTP/1.1 {line}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    }

    fn count(entries: &[DiscussionEntry]) -> usize {
        entries.iter().map(|e| 1 + count(&e.replies)).sum()
    }

    /// A topic with two posters: a reply with a reply, a deleted reply
    /// someone answered, a deleted reply nobody answered, and one newer
    /// reply outside the cached tree.
    const VIEW: &str = r#"{
        "unread_entries": [3, 6],
        "forced_entries": [],
        "entry_ratings": {},
        "participants": [
            {"id": 100, "display_name": "Ada Example"},
            {"id": 200, "display_name": "Ben Example"}
        ],
        "view": [
            {"id": 1, "user_id": 100, "parent_id": null, "created_at": "2026-10-01T09:00:00Z",
             "message": "<p>First <strong>post</strong></p>", "rating_sum": null,
             "replies": [
                {"id": 2, "user_id": 200, "parent_id": 1, "created_at": "2026-10-01T10:00:00Z",
                 "message": "<p>A reply</p>", "rating_sum": 2}
             ]},
            {"id": 3, "parent_id": null, "created_at": "2026-10-02T09:00:00Z", "deleted": true,
             "replies": [
                {"id": 4, "user_id": 100, "parent_id": 3, "created_at": "2026-10-02T10:00:00Z",
                 "message": "<p>Answering a deleted post</p>"}
             ]},
            {"id": 5, "parent_id": null, "created_at": "2026-10-03T09:00:00Z", "deleted": true}
        ],
        "new_entries": [
            {"id": 6, "user_id": 200, "parent_id": 2, "created_at": "2026-10-04T09:00:00Z",
             "message": "<p>Newer</p>"},
            {"id": 2, "user_id": 200, "parent_id": 1, "created_at": "2026-10-01T10:00:00Z",
             "message": "<p>Duplicate of a cached reply</p>"}
        ]
    }"#;

    #[test]
    fn view_becomes_a_named_thread() {
        let entries = threaded(serde_json::from_str(VIEW).unwrap());
        let ids: Vec<u64> = entries.iter().map(|e| e.id).collect();
        assert_eq!(ids, vec![1, 3], "deleted leaf 5 is dropped");

        let first = &entries[0];
        assert_eq!(first.author.as_deref(), Some("Ada Example"));
        assert_eq!(first.body, "First **post**");
        assert!(!first.unread);
        assert_eq!(first.replies.len(), 1, "the duplicate of 2 is skipped");
        let reply = &first.replies[0];
        assert_eq!(reply.body, "A reply", "the cached copy wins");
        assert_eq!(reply.author.as_deref(), Some("Ben Example"));
        assert_eq!(reply.likes, Some(2));
        assert_eq!(reply.replies.len(), 1);
        assert_eq!(reply.replies[0].body, "Newer");
        assert!(reply.replies[0].unread);

        let deleted = &entries[1];
        assert!(deleted.deleted && deleted.unread);
        assert_eq!(
            (deleted.author.as_deref(), deleted.body.as_str()),
            (None, "")
        );
        assert_eq!(deleted.replies[0].body, "Answering a deleted post");
        assert_eq!(count(&entries), 5);
    }

    #[test]
    fn entries_serialise_without_empty_fields() {
        let entries = threaded(serde_json::from_str(VIEW).unwrap());
        let json = serde_json::to_value(&entries[0].replies[0].replies[0]).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "id": 6, "author": "Ben Example", "posted_at": "2026-10-04T09:00:00Z",
                "body": "Newer", "unread": true
            })
        );
    }

    #[test]
    fn orphaned_and_deep_new_entries_are_kept_but_bounded() {
        let mut new_entries = vec![serde_json::json!(
            {"id": 9, "parent_id": 404, "created_at": "2026-10-01T00:00:00Z", "message": "orphan"}
        )];
        // A chain far deeper than MAX_DEPTH must not nest without limit.
        let chain = 2 * MAX_DEPTH as u64;
        for id in 1000..1000 + chain {
            new_entries.push(serde_json::json!({ "id": id, "parent_id": id - 1 }));
        }
        let view: RawView = serde_json::from_value(serde_json::json!({
            "participants": null, "view": [], "new_entries": new_entries
        }))
        .unwrap();
        let entries = threaded(view);
        assert!(entries.iter().any(|e| e.id == 9));
        assert_eq!(count(&entries), 1 + chain as usize, "no entry is lost");
        fn depth(e: &[DiscussionEntry]) -> usize {
            e.iter().map(|e| 1 + depth(&e.replies)).max().unwrap_or(0)
        }
        assert!(depth(&entries) <= MAX_DEPTH, "{}", depth(&entries));
    }

    #[test]
    fn links_name_the_place_and_topic() {
        let course = in_course(71, 569);
        for link in [
            "https://canvas.example.edu/courses/71/discussion_topics/569?module_item_id=4",
            "HTTPS://Canvas.Example.edu/courses/71/discussion_topics/569",
            "canvas.example.edu/courses/71/discussion_topics/569/",
            "/courses/71/discussion_topics/569",
        ] {
            assert_eq!(
                discussion_link(&base(), link).unwrap(),
                Some(course),
                "{link}"
            );
        }
        let group = "https://canvas.example.edu/groups/5/discussion_topics/52";
        assert_eq!(
            discussion_link(&base(), group).unwrap(),
            Some(DiscussionRef::Group {
                group_id: 5,
                topic_id: 52
            })
        );
        for not_a_link in ["569", "Week 3 questions", "http basics", "/r/questions"] {
            assert_eq!(discussion_link(&base(), not_a_link).unwrap(), None);
        }
        for bad in [
            "https://evil.example/courses/71/discussion_topics/569",
            "https://canvas.example.edu/courses/71/pages/intro",
            "https://canvas.example.edu/courses/71/discussion_topics/x",
            "https://canvas.example.edu/users/5/discussion_topics/569",
        ] {
            assert!(discussion_link(&base(), bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn titles_pick_one_topic() {
        let topics = [
            topic(1, "Week 1 questions"),
            topic(2, "Week 2 questions"),
            topic(3, "Week 2"),
            topic(4, "Introductions"),
            topic(5, "introductions "),
        ];
        assert_eq!(pick_discussion(&topics, "week 1").unwrap(), 1);
        assert_eq!(pick_discussion(&topics, "WEEK 2").unwrap(), 3, "exact wins");
        let err = pick_discussion(&topics, "questions")
            .unwrap_err()
            .to_string();
        assert!(err.contains("matches 2 discussions"), "{err}");
        assert!(err.contains(r#"1 "Week 1 questions""#), "{err}");
        assert!(err.contains("third-party"), "{err}");
        let err = pick_discussion(&topics, "Introductions").unwrap_err();
        assert!(err.to_string().contains("matches 2"), "same titles: {err}");
        assert!(pick_discussion(&topics, "exam").is_err());

        let many: Vec<Discussion> = (1..=25).map(|i| topic(i, &format!("Week {i}"))).collect();
        let err = pick_discussion(&many, "week").unwrap_err().to_string();
        assert!(
            err.contains("matches 25") && err.contains("and 15 more"),
            "{err}"
        );
        assert!(!err.contains(r#"11 "Week 11""#), "{err}");
    }

    #[tokio::test]
    async fn empty_names_and_bare_ids_need_more() {
        // No request is made: each fails before reaching Canvas.
        let (base, _) = serve(vec![]).await;
        let c = client(&base);
        for (course, text) in [(Some("1"), "  "), (None, "569"), (None, "week 3")] {
            let err = resolve_discussion(&c, course, text).await.unwrap_err();
            assert!(matches!(err, Error::Input(_)), "{text}: {err}");
        }
    }

    #[tokio::test]
    async fn reads_topic_then_its_replies() {
        let (base, requests) = serve(vec![
            ok_json(
                r#"{"id":5,"title":"Questions","message":"<p>Ask here</p>","author":{"display_name":"Dr Example"},"discussion_subentry_count":3,"user_can_see_posts":true,"group_topic_children":[]}"#,
                None,
            ),
            ok_json(VIEW, None),
        ])
        .await;
        let thread = client(&base).discussion(in_course(1, 5)).await.unwrap();
        assert_eq!(thread.topic.title, "Questions");
        assert_eq!(thread.topic.body, "Ask here");
        assert_eq!(thread.topic.author.as_deref(), Some("Dr Example"));
        assert_eq!(thread.entries.len(), 2);
        assert_eq!(thread.note, None);
        let json = serde_json::to_value(&thread).unwrap();
        assert_eq!(json["untrusted_text"], true);
        assert_eq!(json["replies"], 3);
        assert_eq!(json["entries"][0]["replies"][0]["author"], "Ben Example");
        let requests = requests.await.unwrap();
        assert!(requests[0].starts_with("GET /api/v1/courses/1/discussion_topics/5 "));
        assert!(
            requests[1].starts_with(
                "GET /api/v1/courses/1/discussion_topics/5/view?include_new_entries=1 "
            )
        );
    }

    #[tokio::test]
    async fn post_first_topics_say_so_without_reading_replies() {
        let (base, requests) = serve(vec![ok_json(
            r#"{"id":5,"title":"Introduce yourself","require_initial_post":true,"user_can_see_posts":false}"#,
            None,
        )])
        .await;
        let thread = client(&base).discussion(in_course(1, 5)).await.unwrap();
        assert!(thread.entries.is_empty());
        assert!(thread.note.unwrap().contains("after you post"));
        assert_eq!(requests.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_refused_tree_is_a_note_not_an_error() {
        let (base, _) = serve(vec![
            ok_json(r#"{"id":5,"title":"Introduce yourself"}"#, None),
            status("403 Forbidden"),
        ])
        .await;
        let thread = client(&base).discussion(in_course(1, 5)).await.unwrap();
        assert!(thread.entries.is_empty());
        assert!(thread.note.unwrap().contains("post a reply first"));
    }

    #[tokio::test]
    async fn a_tree_still_being_built_is_polled() {
        let mut responses = vec![ok_json(r#"{"id":5,"title":"Questions"}"#, None)];
        responses.extend((0..MAX_ATTEMPTS).map(|_| status("503 Service Unavailable")));
        responses.push(ok_json(VIEW, None));
        let (base, requests) = serve(responses).await;
        let thread = client(&base).discussion(in_course(1, 5)).await.unwrap();
        assert_eq!(thread.entries.len(), 2);
        assert_eq!(requests.await.unwrap().len(), MAX_ATTEMPTS as usize + 2);
    }

    #[tokio::test]
    async fn group_discussions_read_the_users_copy() {
        // The parent says "post first" (nobody posts there); the user's
        // group copy says they may see replies, and its counts are used.
        let (base, requests) = serve(vec![
            ok_json(
                r#"{"id":5,"title":"Seminar task","discussion_subentry_count":0,"user_can_see_posts":false,"group_topic_children":[{"id":51,"group_id":8},{"id":52,"group_id":9}]}"#,
                None,
            ),
            ok_json(r#"[{"id":9,"name":"Seminar group 2"}]"#, None),
            ok_json(
                r#"{"id":52,"title":"Seminar task","discussion_subentry_count":5,"unread_count":2,"user_can_see_posts":true}"#,
                None,
            ),
            ok_json(VIEW, None),
        ])
        .await;
        let thread = client(&base).discussion(in_course(1, 5)).await.unwrap();
        assert_eq!(thread.entries.len(), 2);
        assert_eq!(thread.note, None);
        assert_eq!((thread.topic.replies, thread.topic.unread), (5, 2));
        let requests = requests.await.unwrap();
        assert!(requests[1].starts_with("GET /api/v1/users/self/groups"));
        assert!(requests[2].starts_with("GET /api/v1/groups/9/discussion_topics/52 "));
        assert!(
            requests[3].starts_with(
                "GET /api/v1/groups/9/discussion_topics/52/view?include_new_entries=1 "
            )
        );
    }

    #[tokio::test]
    async fn group_copies_keep_their_own_post_first_rule() {
        let (base, requests) = serve(vec![
            ok_json(
                r#"{"id":5,"title":"Seminar task","group_topic_children":[{"id":52,"group_id":9}]}"#,
                None,
            ),
            ok_json(r#"[{"id":9}]"#, None),
            ok_json(r#"{"id":52,"title":"Seminar task","user_can_see_posts":false}"#, None),
        ])
        .await;
        let thread = client(&base).discussion(in_course(1, 5)).await.unwrap();
        assert!(thread.entries.is_empty());
        assert!(thread.note.unwrap().contains("after you post"));
        assert_eq!(requests.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn group_discussions_outside_the_users_groups_say_so() {
        let (base, _) = serve(vec![
            ok_json(
                r#"{"id":5,"title":"Seminar task","group_topic_children":[{"id":51,"group_id":8}]}"#,
                None,
            ),
            ok_json("[]", None),
        ])
        .await;
        let thread = client(&base).discussion(in_course(1, 5)).await.unwrap();
        assert!(thread.entries.is_empty());
        assert!(thread.note.unwrap().contains("none of its groups"));
    }

    #[tokio::test]
    async fn group_links_read_that_copy() {
        let (base, requests) = serve(vec![
            ok_json(r#"{"id":52,"title":"Seminar task"}"#, None),
            ok_json(VIEW, None),
        ])
        .await;
        let at = DiscussionRef::Group {
            group_id: 9,
            topic_id: 52,
        };
        let thread = client(&base).discussion(at).await.unwrap();
        assert_eq!(thread.entries.len(), 2);
        let requests = requests.await.unwrap();
        assert!(requests[0].starts_with("GET /api/v1/groups/9/discussion_topics/52 "));
    }

    #[tokio::test]
    async fn anonymous_topics_point_to_the_web_page() {
        let (base, _) = serve(vec![
            status("404 Not Found"),
            ok_json(
                r#"[{"id":5,"title":"Feedback","anonymous_state":"partial_anonymity","html_url":"https://canvas.example.edu/courses/1/discussion_topics/5"}]"#,
                None,
            ),
            status("404 Not Found"),
            ok_json("[]", None),
        ])
        .await;
        let c = client(&base);
        let err = c.discussion(in_course(1, 5)).await.unwrap_err();
        assert!(matches!(err, Error::Forbidden(_)), "{err}");
        assert!(err.to_string().contains("anonymous") && err.to_string().contains("/5"));
        let err = c.discussion(in_course(1, 6)).await.unwrap_err();
        assert!(matches!(err, Error::NotFound(_)), "{err}");
    }

    #[tokio::test]
    async fn a_failed_anonymity_check_is_not_reported_as_not_found() {
        let (base, _) = serve(vec![status("404 Not Found"), status("401 Unauthorized")]).await;
        let err = client(&base).discussion(in_course(1, 5)).await.unwrap_err();
        assert!(matches!(err, Error::SessionExpired), "{err}");
    }
}
