//! Engine for local archive accounts.
//!
//! Fork addition. An archive (`provider = "local"`, see
//! [`local_store::ARCHIVE`]) is mail imported from a file: there is no server
//! to sync with and nothing to log in to. This engine presents the same
//! [`SyncHandle`] as the others so the rest of the app needs no special case,
//! and does two things:
//!
//! - bodies are already stored, so a body request is answered at once;
//! - the op queue is drained locally. Changes to flags and folders were
//!   already applied by the command layer. Anything that has to leave the
//!   machine — a reply, an RSVP, an unsubscribe — is handed to the first real
//!   mail account, because answering a message found in an archive should
//!   just work, and the archive itself cannot send.

use crate::db::models::Account;
use crate::db::Db;
use crate::mail::local_store;
use crate::mail::sync::{SyncCommand, SyncHandle};
use crate::state::AppState;
use serde_json::json;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter, Manager};
use tokio::sync::mpsc;

/// Op kinds that need a server to carry them out.
fn needs_a_mail_account(kind: &str) -> bool {
    matches!(kind, "send" | "rsvp" | "unsubscribe")
}

/// What became of one queued op.
#[derive(Debug, PartialEq, Eq)]
enum Outcome {
    /// Nothing to do on a server; the op is finished.
    Done,
    /// Moved to this account's queue.
    Redirected(String),
    /// Needs a mail account and there is none.
    NoAccount,
}

/// Settle the oldest pending op of an archive account. `None` when the queue
/// is empty.
fn settle_next(
    conn: &mut rusqlite::Connection,
    account_id: &str,
) -> rusqlite::Result<Option<(String, Outcome)>> {
    use rusqlite::OptionalExtension;
    let tx = conn.transaction()?;
    let next: Option<(i64, String, String)> = tx
        .query_row(
            "SELECT id, kind, payload FROM pending_ops
             WHERE account_id = ?1 AND state = 'pending' ORDER BY id LIMIT 1",
            rusqlite::params![account_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((op_id, kind, payload)) = next else {
        return Ok(None);
    };
    if !needs_a_mail_account(&kind) {
        tx.execute(
            "DELETE FROM pending_ops WHERE id = ?1",
            rusqlite::params![op_id],
        )?;
        tx.commit()?;
        return Ok(Some((kind, Outcome::Done)));
    }
    // The oldest account that can actually send. POP3 accounts can.
    let target: Option<String> = tx
        .query_row(
            "SELECT id FROM accounts WHERE provider <> ?1 ORDER BY created_at, id LIMIT 1",
            rusqlite::params![local_store::ARCHIVE],
            |r| r.get(0),
        )
        .optional()?;
    let Some(target) = target else {
        tx.execute(
            "UPDATE pending_ops SET state = 'failed' WHERE id = ?1",
            rusqlite::params![op_id],
        )?;
        tx.commit()?;
        return Ok(Some((kind, Outcome::NoAccount)));
    };
    tx.execute(
        "UPDATE pending_ops SET account_id = ?2 WHERE id = ?1",
        rusqlite::params![op_id, target],
    )?;
    // A draft travels with its send: the sending engine signs it with its own
    // identity and files the copy in its own Sent folder.
    let draft_id = serde_json::from_str::<serde_json::Value>(&payload)
        .ok()
        .and_then(|p| p["draftId"].as_i64());
    if let (true, Some(draft_id)) = (kind == "send", draft_id) {
        tx.execute(
            "UPDATE drafts SET account_id = ?2 WHERE id = ?1",
            rusqlite::params![draft_id, target],
        )?;
    }
    tx.commit()?;
    Ok(Some((kind, Outcome::Redirected(target))))
}

async fn drain_ops(app: &AppHandle, db: &Db, account_id: &str) {
    loop {
        let id = account_id.to_string();
        let settled = db.call(move |conn| settle_next(conn, &id)).await;
        match settled {
            Ok(Some((_, Outcome::Done))) => {}
            Ok(Some((_, Outcome::Redirected(target)))) => {
                let state = app.state::<AppState>();
                let engines = state.engines.lock().await;
                if let Some(handle) = engines.get(&target) {
                    handle.run_ops();
                }
            }
            Ok(Some((kind, Outcome::NoAccount))) => {
                let _ = app.emit(
                    "ops:failed",
                    json!({
                        "kind": kind,
                        "message": "This is a local archive. Connect a mail account to send from it.",
                    }),
                );
            }
            Ok(None) | Err(_) => break,
        }
    }
}

/// Start the engine for an archive account. Mirrors `sync::spawn`.
pub fn spawn(app: AppHandle, db: Db, account: Account, data_dir: PathBuf) -> SyncHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<SyncCommand>();
    let (fetch_tx, mut fetch_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let handle = SyncHandle { tx, fetch_tx };

    tauri::async_runtime::spawn(async move {
        while let Some(cmd) = fetch_rx.recv().await {
            match cmd {
                SyncCommand::Stop => break,
                SyncCommand::FetchBody { respond, .. } => {
                    let _ = respond.send(Ok(()));
                }
                _ => {}
            }
        }
    });

    // An import that was cut short — the app closed, the machine slept — picks
    // up where it stopped. Messages already in are skipped.
    crate::mail::pst_import::resume(&app, &db, &data_dir, &account.id);

    tauri::async_runtime::spawn(async move {
        drain_ops(&app, &db, &account.id).await;
        while let Some(cmd) = rx.recv().await {
            match cmd {
                SyncCommand::Stop => {
                    crate::mail::pst_import::cancel(&account.id);
                    break;
                }
                SyncCommand::RunOps | SyncCommand::SyncAll | SyncCommand::SyncInbox => {
                    drain_ops(&app, &db, &account.id).await;
                }
                _ => {}
            }
        }
    });

    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.with(|conn| {
            conn.execute(
                "INSERT INTO accounts (id, email, provider, imap_host, smtp_host, created_at)
                 VALUES ('arc', 'old.pst', 'local', 'old.pst', '', 5)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        db
    }

    fn add_account(db: &Db, id: &str, provider: &str, created_at: i64) {
        let (id, provider) = (id.to_string(), provider.to_string());
        db.with(move |conn| {
            conn.execute(
                "INSERT INTO accounts (id, email, provider, imap_host, smtp_host, created_at)
                 VALUES (?1, ?1, ?2, 'h', 'h', ?3)",
                rusqlite::params![id, provider, created_at],
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn queue(db: &Db, kind: &str, payload: &str) {
        let (kind, payload) = (kind.to_string(), payload.to_string());
        db.with(move |conn| {
            conn.execute(
                "INSERT INTO pending_ops (account_id, kind, payload, created_at)
                 VALUES ('arc', ?1, ?2, 0)",
                rusqlite::params![kind, payload],
            )?;
            Ok(())
        })
        .unwrap();
    }

    fn settle(db: &Db) -> Option<(String, Outcome)> {
        db.with(|conn| settle_next(conn, "arc")).unwrap()
    }

    #[test]
    fn an_empty_queue_has_nothing_to_settle() {
        assert_eq!(settle(&db()), None);
    }

    #[test]
    fn local_changes_are_simply_finished() {
        let db = db();
        queue(&db, "set_flag", "{}");
        queue(&db, "rename_folder", "{}");
        assert_eq!(settle(&db), Some(("set_flag".into(), Outcome::Done)));
        assert_eq!(settle(&db), Some(("rename_folder".into(), Outcome::Done)));
        assert_eq!(settle(&db), None);
    }

    #[test]
    fn a_reply_is_handed_to_the_oldest_real_account_with_its_draft() {
        let db = db();
        add_account(&db, "newer", "custom", 20);
        add_account(&db, "older", "pop3", 10);
        add_account(&db, "arc2", "local", 1);
        db.with(|conn| {
            conn.execute(
                "INSERT INTO drafts (id, account_id, updated_at) VALUES (7, 'arc', 0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        queue(&db, "send", r#"{"draftId":7}"#);

        assert_eq!(
            settle(&db),
            Some(("send".into(), Outcome::Redirected("older".into())))
        );
        let (op_account, draft_account): (String, String) = db
            .with(|conn| {
                Ok((
                    conn.query_row("SELECT account_id FROM pending_ops", [], |r| r.get(0))?,
                    conn.query_row("SELECT account_id FROM drafts WHERE id = 7", [], |r| {
                        r.get(0)
                    })?,
                ))
            })
            .unwrap();
        assert_eq!(op_account, "older");
        assert_eq!(draft_account, "older");
        assert_eq!(settle(&db), None, "it is no longer the archive's op");
    }

    #[test]
    fn with_no_real_account_the_send_fails_instead_of_waiting_forever() {
        let db = db();
        queue(&db, "send", r#"{"draftId":1}"#);
        assert_eq!(settle(&db), Some(("send".into(), Outcome::NoAccount)));
        let state: String = db
            .with(|conn| conn.query_row("SELECT state FROM pending_ops", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(state, "failed");
        assert_eq!(settle(&db), None);
    }
}
