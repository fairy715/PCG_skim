//! Local-only mailboxes.
//!
//! Fork addition. Upstream Skim assumes every account is an IMAP mailbox: the
//! local database is a cache, and archive/delete/move drop the cached row and
//! wait for the server to show it again in its new folder. An account that has
//! no server-side folders (POP3 today, imported archives later) can't work that
//! way — the local database *is* the mailbox. This module is that other path:
//!
//! - folders are created locally ([`ensure_folders`]),
//! - a raw RFC 822 message is written straight into the database, headers and
//!   body in one go ([`store_raw`]),
//! - archive/delete/junk/move relocate the row instead of dropping it
//!   ([`intercept`], called from `commands::mail::queue_op`).
//!
//! Everything downstream — reading pane, search, threading, AI — reads the same
//! tables as for IMAP and needs no changes.

use crate::db::{bodies, queries, Db};
use crate::error::Result;
use crate::mail::{parse, threading};
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::HashSet;
use std::path::Path;

/// Account `provider` values whose mail lives only in the local database.
pub const POP3: &str = "pop3";
/// An archive with no server at all — mail imported from a file (PST).
pub const ARCHIVE: &str = "local";

pub fn is_local_provider(provider: &str) -> bool {
    provider == POP3 || provider == ARCHIVE
}

/// The fixed folder set of a local mailbox: `(name, role, sort order)`. The
/// name doubles as `imap_name`, which for a local account is just a stable key.
const FOLDERS: &[(&str, &str, i64)] = &[
    ("INBOX", "inbox", 0),
    ("Sent", "sent", 1),
    ("Archive", "archive", 2),
    ("Junk", "junk", 3),
    ("Trash", "trash", 4),
];

/// Create the standard folders of a local account if they are missing. Keyed
/// on the role, not the name: an imported archive brings its own "Deleted
/// Items" or "寄件備份", and must not end up with a second trash next to it.
pub fn ensure_folders(conn: &Connection, account_id: &str) -> rusqlite::Result<()> {
    for (name, role, sort_order) in FOLDERS {
        conn.execute(
            "INSERT OR IGNORE INTO folders (account_id, imap_name, role, display_name, sort_order)
             SELECT ?1, ?2, ?3, ?2, ?4
             WHERE NOT EXISTS (SELECT 1 FROM folders WHERE account_id = ?1 AND role = ?3)",
            params![account_id, name, role, sort_order],
        )?;
    }
    Ok(())
}

/// Sort position of a folder with the given role, matching [`ensure_folders`].
pub fn role_sort_order(role: Option<&str>) -> i64 {
    role.and_then(|r| FOLDERS.iter().find(|(_, role, _)| *role == r))
        .map_or(100, |(_, _, order)| *order)
}

/// This account's folder with the given role.
pub fn folder_by_role(
    conn: &Connection,
    account_id: &str,
    role: &str,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM folders WHERE account_id = ?1 AND role = ?2 ORDER BY id LIMIT 1",
        params![account_id, role],
        |r| r.get(0),
    )
    .optional()
}

/// The next free `uid` in a folder. Local folders have no server numbering, so
/// the uid is only there to satisfy `UNIQUE(folder_id, uid)`.
fn next_uid(conn: &Connection, folder_id: i64) -> rusqlite::Result<u32> {
    conn.query_row(
        "SELECT COALESCE(MAX(uid), 0) + 1 FROM messages WHERE folder_id = ?1",
        params![folder_id],
        |r| r.get(0),
    )
}

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Write one raw message into a local folder — header row, threading, body,
/// attachments and search index — and return its `messages.id`.
///
/// Attachment files are written before the database rows that point at them;
/// a crash in between leaves an orphan directory, never a row without a file.
pub fn store_raw(
    conn: &mut Connection,
    data_dir: &Path,
    account_id: &str,
    folder_id: i64,
    raw: &[u8],
    is_read: bool,
) -> Result<Option<i64>> {
    let size = u32::try_from(raw.len()).ok();
    let msg = parse::parse_headers(
        account_id, folder_id, 0, raw, None, size, is_read, false, false,
    );
    store_message(conn, data_dir, msg, parse::parse_body(raw))
}

/// Write one already-parsed message into a local folder. `msg.account_id` and
/// `msg.folder_id` say where; its `uid` is assigned here. This is the entry
/// point for sources that are not RFC 822 to begin with (a PST import builds
/// the two halves straight from MAPI properties).
pub fn store_message(
    conn: &mut Connection,
    data_dir: &Path,
    mut msg: crate::db::models::NewMessage,
    parsed: parse::ParsedBody,
) -> Result<Option<i64>> {
    msg.uid = next_uid(conn, msg.folder_id)?;
    if msg.date <= 0 {
        // No usable date: arrival time is the best remaining answer.
        msg.date = now_unix();
    }
    let Some((message_pk, _thread)) = queries::insert_message(conn, &msg)? else {
        return Ok(None);
    };

    let dir = data_dir.join("attachments").join(message_pk.to_string());
    let _ = std::fs::remove_dir_all(&dir);
    if !parsed.attachments.is_empty() {
        std::fs::create_dir_all(&dir)?;
    }
    let mut stored = Vec::with_capacity(parsed.attachments.len());
    for (i, a) in parsed.attachments.iter().enumerate() {
        let safe_name = safe_file_name(a.filename.as_deref().unwrap_or("attachment"));
        let path = dir.join(format!("{i}_{safe_name}"));
        std::fs::write(&path, &a.data)?;
        stored.push(bodies::StoredAttachment {
            filename: a.filename.clone(),
            mime_type: a.mime_type.clone(),
            size: a.size,
            content_id: a.content_id.clone(),
            is_inline: a.is_inline,
            cache_path: path.to_string_lossy().into_owned(),
        });
    }
    bodies::set_body(
        conn,
        message_pk,
        parsed.html.as_deref(),
        parsed.text.as_deref(),
        &parsed.snippet,
        &stored,
    )?;
    Ok(Some(message_pk))
}

/// Longest attachment file name kept on disk. Windows caps a whole path at
/// 260 characters by default, and the cache directory takes its share.
const MAX_FILE_NAME_CHARS: usize = 100;

/// A name safe to create inside the attachment cache: nothing a file system
/// reserves, and short enough that a sender's 300-character file name cannot
/// make the write fail. The extension survives the cut.
fn safe_file_name(name: &str) -> String {
    let clean: Vec<char> = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || ".-_ ()".contains(c) {
                c
            } else {
                '_'
            }
        })
        .collect();
    if clean.len() <= MAX_FILE_NAME_CHARS {
        return clean.into_iter().collect();
    }
    let ext_start = clean
        .iter()
        .rposition(|c| *c == '.')
        .filter(|dot| clean.len() - dot <= 12)
        .unwrap_or(clean.len());
    let ext = &clean[ext_start..];
    let keep = MAX_FILE_NAME_CHARS - ext.len();
    clean[..keep].iter().chain(ext).collect()
}

/// Where a local mutation sends its messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Destination {
    Role(&'static str),
    Folder(i64),
}

/// Relocate messages of local accounts. Messages already sitting in the trash
/// that are deleted again are removed for good, attachments included.
/// Returns the attachment directories (message ids) that are now orphaned.
fn relocate(
    conn: &mut Connection,
    message_ids: &[i64],
    dest: Destination,
) -> rusqlite::Result<Vec<i64>> {
    let tx = conn.transaction()?;
    let mut threads = HashSet::new();
    let mut folders = HashSet::new();
    let mut purged = Vec::new();
    for id in message_ids {
        let row: Option<(String, i64, Option<i64>)> = tx
            .query_row(
                "SELECT account_id, folder_id, thread_id FROM messages WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        let Some((account_id, source, thread_id)) = row else {
            continue;
        };
        let target = match dest {
            Destination::Folder(folder_id) => {
                // The destination must belong to the same account.
                let owned: bool = tx
                    .prepare_cached("SELECT 1 FROM folders WHERE id = ?1 AND account_id = ?2")?
                    .exists(params![folder_id, account_id])?;
                if !owned {
                    continue;
                }
                folder_id
            }
            Destination::Role(role) => {
                ensure_folders(&tx, &account_id)?;
                match folder_by_role(&tx, &account_id, role)? {
                    Some(folder_id) => folder_id,
                    None => continue,
                }
            }
        };
        if target == source {
            if dest == Destination::Role("trash") {
                // Deleting from the trash is the permanent delete.
                tx.execute("DELETE FROM messages_fts WHERE rowid = ?1", params![id])?;
                tx.execute("DELETE FROM messages WHERE id = ?1", params![id])?;
                purged.push(*id);
            } else {
                continue;
            }
        } else {
            let uid = next_uid(&tx, target)?;
            tx.execute(
                "UPDATE messages SET folder_id = ?2, uid = ?3 WHERE id = ?1",
                params![id, target, uid],
            )?;
            folders.insert(target);
        }
        folders.insert(source);
        threads.extend(thread_id);
    }
    for thread_id in threads {
        threading::recompute_thread(&tx, thread_id)?;
    }
    for folder_id in folders {
        queries::recompute_folder_unread(&tx, folder_id)?;
    }
    tx.commit()?;
    Ok(purged)
}

/// Split message ids into those owned by local accounts and the rest.
fn partition(conn: &Connection, message_ids: &[i64]) -> rusqlite::Result<(Vec<i64>, Vec<i64>)> {
    let mut stmt = conn.prepare_cached(
        "SELECT a.provider FROM messages m JOIN accounts a ON a.id = m.account_id
         WHERE m.id = ?1",
    )?;
    let (mut local, mut remote) = (Vec::new(), Vec::new());
    for id in message_ids {
        let provider: Option<String> = stmt.query_row(params![id], |r| r.get(0)).optional()?;
        match provider {
            Some(p) if is_local_provider(&p) => local.push(*id),
            _ => remote.push(*id),
        }
    }
    Ok((local, remote))
}

/// The hook in front of the IMAP op queue. For the mutations that move mail
/// (`archive`, `delete`, `junk`, `move`), messages belonging to local accounts
/// are relocated here and now; the ids that still need the IMAP path — every
/// id, for any other kind or when no local account is involved — are returned.
pub async fn intercept(
    db: &Db,
    data_dir: &Path,
    message_ids: Vec<i64>,
    kind: &str,
    extra: &serde_json::Value,
) -> Result<Vec<i64>> {
    let dest = match kind {
        "archive" => Destination::Role("archive"),
        "delete" => Destination::Role("trash"),
        "junk" => Destination::Role("junk"),
        "move" => match extra["destFolderId"].as_i64() {
            Some(folder_id) => Destination::Folder(folder_id),
            None => return Ok(message_ids),
        },
        _ => return Ok(message_ids),
    };
    let (remote, purged) = db
        .call(move |conn| {
            let (local, remote) = partition(conn, &message_ids)?;
            let purged = if local.is_empty() {
                Vec::new()
            } else {
                relocate(conn, &local, dest)?
            };
            Ok((remote, purged))
        })
        .await?;
    for id in purged {
        let _ = std::fs::remove_dir_all(data_dir.join("attachments").join(id.to_string()));
    }
    Ok(remote)
}

#[cfg(test)]
mod tests {
    use super::*;

    const RAW: &[u8] = b"From: Ann <ann@example.com>\r\nTo: me@example.com\r\n\
Subject: Hello there\r\nMessage-ID: <one@example.com>\r\n\
Date: Tue, 03 Mar 2026 10:00:00 +0000\r\n\r\nFirst line of the body.\r\n";

    fn db_with_account(provider: &str) -> Db {
        let db = Db::open_in_memory().unwrap();
        let provider = provider.to_string();
        db.with(move |conn| {
            conn.execute(
                "INSERT INTO accounts (id, email, provider, imap_host, smtp_host, created_at)
                 VALUES ('acc', 'me@example.com', ?1, 'pop.example.com', 'smtp.example.com', 0)",
                params![provider],
            )?;
            ensure_folders(conn, "acc")
        })
        .unwrap();
        db
    }

    fn folder(db: &Db, role: &'static str) -> i64 {
        db.with(|conn| folder_by_role(conn, "acc", role))
            .unwrap()
            .unwrap()
    }

    fn store(db: &Db, dir: &Path, folder_id: i64) -> i64 {
        let mut pk = None;
        db.with(|conn| {
            pk = store_raw(conn, dir, "acc", folder_id, RAW, false).unwrap();
            Ok(())
        })
        .unwrap();
        pk.unwrap()
    }

    fn folder_of(db: &Db, pk: i64) -> Option<i64> {
        db.with(|conn| {
            conn.query_row(
                "SELECT folder_id FROM messages WHERE id = ?1",
                params![pk],
                |r| r.get(0),
            )
            .optional()
        })
        .unwrap()
    }

    fn unread(db: &Db, folder_id: i64) -> i64 {
        db.with(|conn| {
            conn.query_row(
                "SELECT unread_count FROM folders WHERE id = ?1",
                params![folder_id],
                |r| r.get(0),
            )
        })
        .unwrap()
    }

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("skim-local-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn file_names_are_made_safe_and_kept_short() {
        assert_eq!(safe_file_name("報告 (final).xlsx"), "報告 (final).xlsx");
        assert_eq!(safe_file_name("a/b\\c:d*.txt"), "a_b_c_d_.txt");
        let long = format!("{}.pdf", "x".repeat(400));
        let safe = safe_file_name(&long);
        assert_eq!(safe.chars().count(), MAX_FILE_NAME_CHARS);
        assert!(safe.ends_with(".pdf"));
        // No extension worth keeping: just cut.
        assert_eq!(
            safe_file_name(&"y".repeat(400)).chars().count(),
            MAX_FILE_NAME_CHARS
        );
    }

    #[test]
    fn folders_are_created_once() {
        let db = db_with_account(POP3);
        db.with(|conn| ensure_folders(conn, "acc")).unwrap();
        let count: i64 = db
            .with(|conn| conn.query_row("SELECT count(*) FROM folders", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(count, FOLDERS.len() as i64);
    }

    #[test]
    fn a_stored_message_is_readable_and_searchable() {
        let db = db_with_account(POP3);
        let dir = temp_dir("store");
        let inbox = folder(&db, "inbox");
        let pk = store(&db, &dir, inbox);

        let (subject, from, state, date): (String, String, i64, i64) = db
            .with(|conn| {
                conn.query_row(
                    "SELECT subject, from_addr, body_state, date FROM messages WHERE id = ?1",
                    params![pk],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                )
            })
            .unwrap();
        assert_eq!(subject, "Hello there");
        assert_eq!(from, "ann@example.com");
        assert_eq!(state, 1, "the body is stored with the message");
        assert!(date > 0);

        let (_html, text) = db
            .with(|conn| bodies::get_body(conn, pk))
            .unwrap()
            .expect("body row");
        assert!(text.unwrap_or_default().contains("First line"));

        let hits: i64 = db
            .with(|conn| {
                conn.query_row(
                    "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'body'",
                    [],
                    |r| r.get(0),
                )
            })
            .unwrap();
        assert_eq!(hits, 1);
        assert_eq!(unread(&db, inbox), 1);
    }

    #[test]
    fn each_stored_message_gets_its_own_uid() {
        let db = db_with_account(POP3);
        let dir = temp_dir("uid");
        let inbox = folder(&db, "inbox");
        let a = store(&db, &dir, inbox);
        let b = store(&db, &dir, inbox);
        assert_ne!(a, b);
    }

    #[tokio::test]
    async fn archive_moves_the_message_instead_of_dropping_it() {
        let db = db_with_account(POP3);
        let dir = temp_dir("archive");
        let inbox = folder(&db, "inbox");
        let pk = store(&db, &dir, inbox);

        let left = intercept(&db, &dir, vec![pk], "archive", &serde_json::json!({}))
            .await
            .unwrap();
        assert!(left.is_empty(), "nothing is left for the IMAP queue");
        assert_eq!(folder_of(&db, pk), Some(folder(&db, "archive")));
        assert_eq!(unread(&db, inbox), 0);
        assert_eq!(unread(&db, folder(&db, "archive")), 1);
    }

    #[tokio::test]
    async fn delete_goes_to_trash_and_a_second_delete_is_permanent() {
        let db = db_with_account(POP3);
        let dir = temp_dir("delete");
        let pk = store(&db, &dir, folder(&db, "inbox"));
        let attachments = dir.join("attachments").join(pk.to_string());
        std::fs::create_dir_all(&attachments).unwrap();

        intercept(&db, &dir, vec![pk], "delete", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(folder_of(&db, pk), Some(folder(&db, "trash")));
        assert!(attachments.exists());

        intercept(&db, &dir, vec![pk], "delete", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(folder_of(&db, pk), None);
        assert!(!attachments.exists(), "its attachments go with it");
        let threads: i64 = db
            .with(|conn| conn.query_row("SELECT count(*) FROM threads", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(threads, 0);
    }

    #[tokio::test]
    async fn move_lands_in_the_chosen_folder() {
        let db = db_with_account(POP3);
        let dir = temp_dir("move");
        let pk = store(&db, &dir, folder(&db, "inbox"));
        let dest: i64 = db
            .with(|conn| {
                conn.execute(
                    "INSERT INTO folders (account_id, imap_name, display_name, sort_order)
                     VALUES ('acc', 'Projects', 'Projects', 100)",
                    [],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .unwrap();
        let extra = serde_json::json!({ "destFolderId": dest });
        let left = intercept(&db, &dir, vec![pk], "move", &extra)
            .await
            .unwrap();
        assert!(left.is_empty());
        assert_eq!(folder_of(&db, pk), Some(dest));
    }

    #[tokio::test]
    async fn imap_accounts_and_flag_changes_pass_through_untouched() {
        let db = db_with_account("custom");
        let dir = temp_dir("imap");
        let inbox = folder(&db, "inbox");
        let pk = store(&db, &dir, inbox);

        let left = intercept(&db, &dir, vec![pk], "archive", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(left, vec![pk], "an IMAP message still goes to the op queue");
        assert_eq!(folder_of(&db, pk), Some(inbox));

        let db = db_with_account(POP3);
        let pk = store(&db, &dir, folder(&db, "inbox"));
        let left = intercept(&db, &dir, vec![pk], "set_flag", &serde_json::json!({}))
            .await
            .unwrap();
        assert_eq!(left, vec![pk], "flags keep their existing local path");
    }
}
