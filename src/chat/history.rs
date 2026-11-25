//! The chat history on disk: one SQLite row per message, read a page at a
//! time, so a conversation of any length opens as fast as a short one and a
//! new message writes only itself. Each message is kept as its JSON (the
//! `Message` serde form), with the columns the queries order and filter by.

use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;

use super::{ConvId, Message};

/// The database, in the chat folder.
const FILE: &str = "history.db";
/// Where the per-conversation JSON files the history was kept in before go
/// once copied in (kept, not deleted).
const OLD_FILES: &str = "json-before-history";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS conversation (
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    unread INTEGER NOT NULL DEFAULT 0,
    pinned TEXT,
    PRIMARY KEY (kind, name)
) WITHOUT ROWID;
CREATE TABLE IF NOT EXISTS message (
    seq INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    name TEXT NOT NULL,
    id TEXT NOT NULL,
    at INTEGER NOT NULL,
    mine INTEGER NOT NULL,
    body TEXT NOT NULL,
    UNIQUE (kind, name, id)
);
CREATE INDEX IF NOT EXISTS message_order ON message (kind, name, at, seq);
";

/// A conversation as the JSON files kept it.
#[derive(Deserialize)]
struct OldConversation {
    messages: Vec<Message>,
    #[serde(default)]
    unread: u32,
    #[serde(default)]
    pinned: Option<String>,
}

/// Where a page starts, at a message's (at, seq).
enum Start {
    Newest,
    Before((i64, i64)),
    Since((i64, i64)),
}

pub(super) struct History {
    db: Connection,
}

/// A failed read or write is reported and the history goes on without it:
/// what's on screen stays, and the next change writes again.
fn logged<T: Default>(what: &str, r: rusqlite::Result<T>) -> T {
    r.unwrap_or_else(|e| {
        // allow-noisy-log: a history that can't be written says why, as the rest of the app reports failures.
        eprintln!("[biglace] chat history: {what}: {e}");
        T::default()
    })
}

fn at_column(at: u64) -> i64 {
    i64::try_from(at).unwrap_or(i64::MAX)
}

fn decode(body: &str) -> Option<Message> {
    serde_json::from_str(body).ok()
}

impl History {
    /// The history under `dir`, with what the JSON files there held copied in.
    pub fn open(dir: &Path) -> rusqlite::Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir);
        let path = dir.join(FILE);
        // Made private before SQLite opens it: its journal files take its mode.
        let _ = std::fs::OpenOptions::new().write(true).create(true).truncate(false).mode(0o600).open(&path);
        let db = Connection::open(&path)?;
        db.pragma_update(None, "journal_mode", "WAL")?;
        db.pragma_update(None, "synchronous", "NORMAL")?;
        db.execute_batch(SCHEMA)?;
        let mut history = Self { db };
        history.take_old_files(dir)?;
        Ok(history)
    }

    /// A history kept in memory alone, when the one on disk can't be opened.
    pub fn in_memory() -> Self {
        let db = Connection::open_in_memory().expect("an in-memory database opens");
        db.execute_batch(SCHEMA).expect("the schema applies to an empty database");
        Self { db }
    }

    /// Copy the conversations kept as `<kind>-<name>.json` in, then move the
    /// files aside. Copying again is harmless: what's there already stays.
    fn take_old_files(&mut self, dir: &Path) -> rusqlite::Result<()> {
        let mut files: Vec<(ConvId, PathBuf)> = Vec::new();
        for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Some((kind, conv)) = name.strip_suffix(".json").and_then(|stem| stem.split_once('-')) else { continue };
            let id = ConvId { kind: kind.into(), name: conv.into() };
            if id.valid() {
                files.push((id, entry.path()));
            }
        }
        if files.is_empty() {
            return Ok(());
        }
        let tx = self.db.transaction()?;
        let mut copied = Vec::new();
        for (id, path) in files {
            let Ok(Ok(old)) = std::fs::read(&path).map(|b| serde_json::from_slice::<OldConversation>(&b)) else {
                // allow-noisy-log: a conversation file that can't be read stays where it is, and says so.
                eprintln!("[biglace] chat history: {} can't be read; left as it is", path.display());
                continue;
            };
            tx.execute(
                "INSERT OR IGNORE INTO conversation (kind, name, unread, pinned) VALUES (?1, ?2, ?3, ?4)",
                params![id.kind, id.name, old.unread, old.pinned],
            )?;
            for m in &old.messages {
                insert(&tx, &id, m)?;
            }
            copied.push(path);
        }
        tx.commit()?;
        // Only what went in moves aside, never over an earlier copy of the same name.
        let aside = dir.join(OLD_FILES);
        let _ = std::fs::create_dir_all(&aside);
        for path in copied {
            let Some(name) = path.file_name() else { continue };
            let mut target = aside.join(name);
            let mut n = 1;
            while target.exists() {
                target = aside.join(format!("{}.{n}", name.to_string_lossy()));
                n += 1;
            }
            let _ = std::fs::rename(&path, target);
        }
        Ok(())
    }

    /// Make sure `conv` exists (an empty one shows in the list).
    pub fn ensure(&self, conv: &ConvId) {
        logged(
            "add a conversation",
            self.db
                .prepare_cached("INSERT OR IGNORE INTO conversation (kind, name) VALUES (?1, ?2)")
                .and_then(|mut s| s.execute(params![conv.kind, conv.name]))
                .map(drop),
        );
    }

    /// Add `m` to `conv` unless it's there already; true if added.
    pub fn add(&self, conv: &ConvId, m: &Message, count_unread: bool) -> bool {
        self.ensure(conv);
        let added = logged("add a message", insert(&self.db, conv, m)) > 0;
        if added && count_unread {
            logged(
                "count it unread",
                self.db
                    .prepare_cached("UPDATE conversation SET unread = unread + 1 WHERE kind = ?1 AND name = ?2")
                    .and_then(|mut s| s.execute(params![conv.kind, conv.name]))
                    .map(drop),
            );
        }
        added
    }

    /// Write `m` over the message of `conv` with its id.
    pub fn put(&self, conv: &ConvId, m: &Message) {
        let Ok(body) = serde_json::to_string(m) else { return };
        logged(
            "change a message",
            self.db
                .prepare_cached("UPDATE message SET body = ?4 WHERE kind = ?1 AND name = ?2 AND id = ?3")
                .and_then(|mut s| s.execute(params![conv.kind, conv.name, m.id, body]))
                .map(drop),
        );
    }

    pub fn message(&self, conv: &ConvId, id: &str) -> Option<Message> {
        let body: Option<String> = logged(
            "read a message",
            self.db
                .prepare_cached("SELECT body FROM message WHERE kind = ?1 AND name = ?2 AND id = ?3")
                .and_then(|mut s| s.query_row(params![conv.kind, conv.name, id], |r| r.get(0)).optional()),
        );
        decode(&body?)
    }

    /// Every conversation with its unread count.
    pub fn conversations(&self) -> Vec<(ConvId, u32)> {
        logged(
            "list the conversations",
            self.db.prepare_cached("SELECT kind, name, unread FROM conversation").and_then(|mut s| {
                s.query_map([], |r| Ok((ConvId { kind: r.get(0)?, name: r.get(1)? }, r.get(2)?)))?.collect()
            }),
        )
    }

    pub fn unread(&self, conv: &ConvId) -> u32 {
        logged(
            "read the unread count",
            self.db
                .prepare_cached("SELECT unread FROM conversation WHERE kind = ?1 AND name = ?2")
                .and_then(|mut s| s.query_row(params![conv.kind, conv.name], |r| r.get(0)).optional())
                .map(Option::unwrap_or_default),
        )
    }

    pub fn set_unread(&self, conv: &ConvId, unread: u32) {
        logged(
            "set the unread count",
            self.db
                .prepare_cached("UPDATE conversation SET unread = ?3 WHERE kind = ?1 AND name = ?2")
                .and_then(|mut s| s.execute(params![conv.kind, conv.name, unread]))
                .map(drop),
        );
    }

    /// Someone else's messages written after `at`.
    pub fn received_after(&self, conv: &ConvId, at: u64) -> u32 {
        logged(
            "count the messages read later",
            self.db
                .prepare_cached("SELECT count(*) FROM message WHERE kind = ?1 AND name = ?2 AND mine = 0 AND at > ?3")
                .and_then(|mut s| s.query_row(params![conv.kind, conv.name, at_column(at)], |r| r.get(0))),
        )
    }

    /// When the newest of someone else's messages was written.
    pub fn last_received_at(&self, conv: &ConvId) -> Option<u64> {
        let at: Option<i64> = logged(
            "find the last message received",
            self.db
                .prepare_cached("SELECT max(at) FROM message WHERE kind = ?1 AND name = ?2 AND mine = 0")
                .and_then(|mut s| s.query_row(params![conv.kind, conv.name], |r| r.get(0))),
        );
        at.and_then(|at| u64::try_from(at).ok())
    }

    pub fn pinned(&self, conv: &ConvId) -> Option<String> {
        logged(
            "read the pinned message",
            self.db
                .prepare_cached("SELECT pinned FROM conversation WHERE kind = ?1 AND name = ?2")
                .and_then(|mut s| s.query_row(params![conv.kind, conv.name], |r| r.get(0)).optional())
                .map(Option::flatten),
        )
    }

    pub fn set_pinned(&self, conv: &ConvId, id: Option<&str>) {
        logged(
            "pin a message",
            self.db
                .prepare_cached("UPDATE conversation SET pinned = ?3 WHERE kind = ?1 AND name = ?2")
                .and_then(|mut s| s.execute(params![conv.kind, conv.name, id]))
                .map(drop),
        );
    }

    /// Where message `id` of `conv` sits in its order: (at, seq).
    fn place(&self, conv: &ConvId, id: &str) -> Option<(i64, i64)> {
        logged(
            "find a message",
            self.db
                .prepare_cached("SELECT at, seq FROM message WHERE kind = ?1 AND name = ?2 AND id = ?3")
                .and_then(|mut s| s.query_row(params![conv.kind, conv.name, id], |r| Ok((r.get(0)?, r.get(1)?))).optional()),
        )
    }

    /// The last message of `conv`.
    pub fn last(&self, conv: &ConvId) -> Option<Message> {
        self.recent(conv, 1).pop()
    }

    /// The newest `n` messages of `conv`, oldest first.
    pub fn recent(&self, conv: &ConvId, n: usize) -> Vec<Message> {
        let mut page = self.page(conv, Start::Newest, n);
        page.reverse();
        page
    }

    /// The `n` messages of `conv` just before message `id`, oldest first;
    /// none when `id` isn't there.
    pub fn before(&self, conv: &ConvId, id: &str, n: usize) -> Vec<Message> {
        let Some((at, seq)) = self.place(conv, id) else { return vec![] };
        let mut page = self.page(conv, Start::Before((at, seq)), n);
        page.reverse();
        page
    }

    /// Newest first, up to `n`, from where `start` says.
    fn page(&self, conv: &ConvId, start: Start, n: usize) -> Vec<Message> {
        let (sql, (at, seq)) = match start {
            Start::Newest => ("SELECT body FROM message WHERE kind = ?1 AND name = ?2 AND ?3 = ?4 ORDER BY at DESC, seq DESC LIMIT ?5", (0, 0)),
            Start::Before(place) => (
                "SELECT body FROM message WHERE kind = ?1 AND name = ?2 AND (at < ?3 OR (at = ?3 AND seq < ?4)) ORDER BY at DESC, seq DESC LIMIT ?5",
                place,
            ),
            Start::Since(place) => (
                "SELECT body FROM message WHERE kind = ?1 AND name = ?2 AND (at > ?3 OR (at = ?3 AND seq >= ?4)) ORDER BY at DESC, seq DESC LIMIT ?5",
                place,
            ),
        };
        let limit = i64::try_from(n).unwrap_or(i64::MAX);
        let bodies: Vec<String> = logged(
            "read a page of messages",
            self.db
                .prepare_cached(sql)
                .and_then(|mut s| s.query_map(params![conv.kind, conv.name, at, seq, limit], |r| r.get(0))?.collect()),
        );
        bodies.iter().filter_map(|b| decode(b)).collect()
    }

    /// Message `id` of `conv` and everything after it, oldest first; none
    /// when `id` isn't there.
    pub fn since(&self, conv: &ConvId, id: &str) -> Vec<Message> {
        let Some((at, seq)) = self.place(conv, id) else { return vec![] };
        let mut page = self.page(conv, Start::Since((at, seq)), usize::MAX);
        page.reverse();
        page
    }

    /// Every message of `conv`, oldest first, one at a time.
    pub fn each(&self, conv: &ConvId, mut f: impl FnMut(&Message)) {
        logged(
            "read the messages",
            self.db
                .prepare("SELECT body FROM message WHERE kind = ?1 AND name = ?2 ORDER BY at, seq")
                .and_then(|mut s| {
                    let mut rows = s.query(params![conv.kind, conv.name])?;
                    while let Some(row) = rows.next()? {
                        if let Some(m) = decode(row.get_ref(0)?.as_str()?) {
                            f(&m);
                        }
                    }
                    Ok(())
                }),
        );
    }

    /// `ids` of `conv` in the conversation's order (the ones not there left out).
    pub fn in_order(&self, conv: &ConvId, ids: &[String]) -> Vec<String> {
        let mut placed: Vec<((i64, i64), &String)> = ids.iter().filter_map(|id| Some((self.place(conv, id)?, id))).collect();
        placed.sort();
        placed.into_iter().map(|(_, id)| id.clone()).collect()
    }

    /// Remove messages `ids` of `conv` (unpinned if one was pinned).
    pub fn remove(&self, conv: &ConvId, ids: &[String]) {
        for id in ids {
            logged(
                "remove a message",
                self.db
                    .prepare_cached("DELETE FROM message WHERE kind = ?1 AND name = ?2 AND id = ?3")
                    .and_then(|mut s| s.execute(params![conv.kind, conv.name, id]))
                    .map(drop),
            );
        }
        if self.pinned(conv).is_some_and(|p| ids.contains(&p)) {
            self.set_pinned(conv, None);
        }
    }

    /// Forget `conv` and its messages; a new message starts it again.
    pub fn drop_conversation(&mut self, conv: &ConvId) {
        logged("remove a conversation", self.try_drop(conv));
    }

    fn try_drop(&mut self, conv: &ConvId) -> rusqlite::Result<()> {
        let tx = self.db.transaction()?;
        tx.execute("DELETE FROM message WHERE kind = ?1 AND name = ?2", params![conv.kind, conv.name])?;
        tx.execute("DELETE FROM conversation WHERE kind = ?1 AND name = ?2", params![conv.kind, conv.name])?;
        tx.commit()
    }

    /// Conversation `old` is called `new` now: its messages join any `new`
    /// already has (without repeats), with its unread count and pin. False
    /// when there was no `old`.
    pub fn rename(&mut self, old: &ConvId, new: &ConvId) -> bool {
        logged("rename a conversation", self.try_rename(old, new))
    }

    fn try_rename(&mut self, old: &ConvId, new: &ConvId) -> rusqlite::Result<bool> {
        let tx = self.db.transaction()?;
        let moved: Option<(u32, Option<String>)> = tx
            .query_row("SELECT unread, pinned FROM conversation WHERE kind = ?1 AND name = ?2", params![old.kind, old.name], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let Some((unread, pinned)) = moved else { return Ok(false) };
        tx.execute("INSERT OR IGNORE INTO conversation (kind, name) VALUES (?1, ?2)", params![new.kind, new.name])?;
        tx.execute(
            "UPDATE conversation SET unread = unread + ?3, pinned = coalesce(pinned, ?4) WHERE kind = ?1 AND name = ?2",
            params![new.kind, new.name, unread, pinned],
        )?;
        tx.execute(
            "UPDATE OR IGNORE message SET kind = ?3, name = ?4 WHERE kind = ?1 AND name = ?2",
            params![old.kind, old.name, new.kind, new.name],
        )?;
        tx.execute("DELETE FROM message WHERE kind = ?1 AND name = ?2", params![old.kind, old.name])?;
        tx.execute("DELETE FROM conversation WHERE kind = ?1 AND name = ?2", params![old.kind, old.name])?;
        tx.commit()?;
        Ok(true)
    }
}

/// Insert `m` into `conv` unless its id is there; how many rows went in.
fn insert(db: &Connection, conv: &ConvId, m: &Message) -> rusqlite::Result<usize> {
    let body = serde_json::to_string(m).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
    db.prepare_cached("INSERT OR IGNORE INTO message (kind, name, id, at, mine, body) VALUES (?1, ?2, ?3, ?4, ?5, ?6)")?
        .execute(params![conv.kind, conv.name, m.id, at_column(m.at), m.mine, body])
}
