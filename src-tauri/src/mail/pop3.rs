//! POP3 account engine.
//!
//! Fork addition. Presents the same [`SyncHandle`] as the IMAP engine in
//! `sync.rs`, so the rest of the app (commands, state, UI) does not know the
//! difference — but behind it there is no folder sync, no flag reconciliation
//! and no IDLE. A pass logs in, asks which messages the maildrop holds, downloads
//! the ones not seen before into the local inbox ([`local_store::store_raw`]),
//! and logs out.
//!
//! Receive-only by design: nothing is ever deleted from, or written back to,
//! the POP3 server. Read state, archive, delete and folders live in the local
//! database alone. Sending goes out over SMTP like any other account, and the
//! sent copy is filed in the local Sent folder.

use crate::db::models::Account;
use crate::db::{accounts as db_accounts, Db};
use crate::error::{Result, SkimError};
use crate::mail::sync::{SyncCommand, SyncHandle};
use crate::mail::{imap_client, local_store, pop3_client, smtp};
use crate::secrets;
use serde_json::json;
use std::collections::HashSet;
use std::path::PathBuf;
use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;

/// No IDLE in POP3, so this poll is the only way new mail is noticed.
const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_secs(180);
/// Messages downloaded per pass. A first sync of a deep maildrop runs as many
/// short passes rather than one long one, so sending, stopping and the UI are
/// never stuck behind it; the next pass starts as soon as this one ends.
const BATCH: usize = 50;
/// An op that keeps failing for a non-transient reason is given up on.
const MAX_ATTEMPTS: i64 = 5;

/// Create the table that remembers which maildrop entries were downloaded.
/// Kept out of the numbered migrations on purpose: those belong to upstream,
/// and a fork-owned number would collide with its next one.
pub fn ensure_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    local_store::ensure_schema(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pop3_seen (
           account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
           uidl       TEXT NOT NULL,
           PRIMARY KEY (account_id, uidl)
         ) WITHOUT ROWID;",
    )
}

/// Which maildrop entries to download now, newest first, and whether more are
/// left for a later pass. `seen` holds the unique ids already downloaded.
fn plan_batch(
    listing: &[pop3_client::Listing],
    seen: &HashSet<String>,
    batch: usize,
) -> (Vec<pop3_client::Listing>, bool) {
    let mut fresh: Vec<pop3_client::Listing> = listing
        .iter()
        .filter(|l| !seen.contains(&l.uidl))
        .cloned()
        .collect();
    // Higher message numbers arrived later; the user wants those first.
    fresh.sort_by_key(|l| std::cmp::Reverse(l.number));
    let more = fresh.len() > batch;
    fresh.truncate(batch);
    (fresh, more)
}

/// Ids we remember that the server no longer lists. Forgetting them keeps the
/// table from growing forever on a server that expires old mail.
fn stale_ids(listing: &[pop3_client::Listing], seen: &HashSet<String>) -> Vec<String> {
    let on_server: HashSet<&str> = listing.iter().map(|l| l.uidl.as_str()).collect();
    seen.iter()
        .filter(|id| !on_server.contains(id.as_str()))
        .cloned()
        .collect()
}

struct Engine {
    app: AppHandle,
    db: Db,
    account: Account,
    data_dir: PathBuf,
}

/// Outcome of one download pass.
struct Pass {
    /// Newly stored messages.
    stored: Vec<i64>,
    /// The maildrop still holds mail this pass did not get to.
    more: bool,
}

impl Engine {
    fn emit_status(&self, state: &str, message: Option<String>) {
        let _ = self.app.emit(
            "sync:status",
            json!({ "accountId": self.account.id, "state": state, "message": message }),
        );
    }

    async fn refresh_account(&mut self) {
        let id = self.account.id.clone();
        if let Ok(Some(fresh)) = self.db.call(move |conn| db_accounts::get(conn, &id)).await {
            self.account = fresh;
        }
    }

    fn password(&self) -> Result<String> {
        secrets::get(&secrets::mail_key(&self.account.id))?
            .ok_or_else(|| SkimError::other("auth", "no stored credentials for this account"))
    }

    /// Returns `(inbox folder id, initial download finished, ids already seen)`.
    async fn local_state(&self) -> Result<(i64, bool, HashSet<String>)> {
        let account_id = self.account.id.clone();
        self.db
            .call(move |conn| {
                ensure_schema(conn)?;
                local_store::ensure_folders(conn, &account_id)?;
                let (inbox, done): (i64, bool) = conn.query_row(
                    "SELECT id, backfill_done FROM folders
                     WHERE account_id = ?1 AND role = 'inbox' ORDER BY id LIMIT 1",
                    rusqlite::params![account_id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?;
                let mut stmt = conn.prepare("SELECT uidl FROM pop3_seen WHERE account_id = ?1")?;
                let seen = stmt
                    .query_map(rusqlite::params![account_id], |r| r.get::<_, String>(0))?
                    .collect::<rusqlite::Result<HashSet<_>>>()?;
                Ok((inbox, done, seen))
            })
            .await
    }

    async fn download(&mut self) -> Result<Pass> {
        let (inbox, initial_done, seen) = self.local_state().await?;
        let password = self.password()?;
        let mut session = pop3_client::login(
            &self.account.imap_host,
            self.account.imap_port,
            self.account.login_user(),
            &password,
        )
        .await?;
        let listing = session.uidl().await?;
        let (batch, more) = plan_batch(&listing, &seen, BATCH);
        let stale = stale_ids(&listing, &seen);
        let total = batch.len();

        let mut stored = Vec::new();
        let mut failure = None;
        for (done, entry) in batch.into_iter().enumerate() {
            let raw = match session.retr(entry.number).await {
                Ok(raw) => raw,
                Err(e) => {
                    failure = Some(e);
                    break;
                }
            };
            let account_id = self.account.id.clone();
            let data_dir = self.data_dir.clone();
            let outcome = self
                .db
                .call(move |conn| {
                    let pk = match local_store::store_raw(
                        conn,
                        &data_dir,
                        &account_id,
                        inbox,
                        &raw,
                        false,
                    ) {
                        Ok(pk) => pk,
                        Err(e) => return Ok(Err(e)),
                    };
                    // Recorded only once the message is safely stored: a crash
                    // in between costs one duplicate, never a lost message.
                    conn.execute(
                        "INSERT OR IGNORE INTO pop3_seen (account_id, uidl) VALUES (?1, ?2)",
                        rusqlite::params![account_id, entry.uidl],
                    )?;
                    Ok(Ok(pk))
                })
                .await;
            match outcome {
                Ok(Ok(pk)) => stored.extend(pk),
                Ok(Err(e)) | Err(e) => {
                    failure = Some(e);
                    break;
                }
            }
            let _ = self.app.emit(
                "sync:progress",
                json!({ "folderId": inbox, "done": done + 1, "total": total }),
            );
        }
        session.quit().await;

        // What was stored before a failure is real and should show up now.
        if !stored.is_empty() {
            let _ = self.app.emit("mail:updated", json!({ "folderId": inbox }));
        }
        if let Some(e) = failure {
            return Err(e);
        }

        let account_id = self.account.id.clone();
        let finished = !more;
        self.db
            .call(move |conn| {
                let tx = conn.transaction()?;
                for id in &stale {
                    tx.execute(
                        "DELETE FROM pop3_seen WHERE account_id = ?1 AND uidl = ?2",
                        rusqlite::params![account_id, id],
                    )?;
                }
                if finished {
                    tx.execute(
                        "UPDATE folders SET backfill_done = 1 WHERE id = ?1",
                        rusqlite::params![inbox],
                    )?;
                }
                tx.commit()
            })
            .await?;

        // Mail that was already in the maildrop when the account was added is
        // history, not news: only arrivals after the first full download toast.
        if initial_done && !stored.is_empty() {
            let _ = self.app.emit("mail:new", json!({ "count": stored.len() }));
            crate::notify::notify_new_mail(&self.app, &self.db, &stored).await;
        }
        Ok(Pass { stored, more })
    }

    /// One pass. Returns whether another should follow straight away.
    async fn run_sync(&mut self) -> bool {
        self.emit_status("syncing", None);
        let again = match self.download().await {
            Ok(pass) => {
                self.emit_status("idle", None);
                // `stored` guards against spinning on a maildrop whose entries
                // can never be stored: no progress, no immediate retry.
                pass.more && !pass.stored.is_empty()
            }
            Err(e) => {
                tracing::warn!(error = %e, "POP3 sync failed");
                self.emit_status("error", Some(e.to_string()));
                false
            }
        };
        crate::badge::refresh(&self.app).await;
        again
    }

    // ---- offline op queue ----------------------------------------------

    async fn drain_ops(&mut self) {
        loop {
            let account_id = self.account.id.clone();
            let next: Option<(i64, String, String, i64)> = match self
                .db
                .call(move |conn| {
                    use rusqlite::OptionalExtension;
                    conn.query_row(
                        "SELECT id, kind, payload, attempts FROM pending_ops
                         WHERE account_id = ?1 AND state = 'pending' ORDER BY id LIMIT 1",
                        rusqlite::params![account_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()
                })
                .await
            {
                Ok(v) => v,
                Err(_) => break,
            };
            let Some((op_id, kind, payload, attempts)) = next else {
                break;
            };
            self.refresh_account().await;

            let parsed: serde_json::Value = match serde_json::from_str(&payload) {
                Ok(v) => v,
                Err(_) => {
                    let _ = self.finish_op(op_id, false).await;
                    continue;
                }
            };
            match self.execute_op(&kind, &parsed).await {
                Ok(()) => {
                    let _ = self.finish_op(op_id, true).await;
                }
                Err(e) => {
                    tracing::warn!(op = %kind, error = %e, "op failed");
                    // Transient — leave it queued and retry on the next drain.
                    if matches!(e.code(), "network" | "tls") {
                        break;
                    }
                    if attempts + 1 >= MAX_ATTEMPTS {
                        let _ = self.finish_op(op_id, false).await;
                        let _ = self.app.emit(
                            "ops:failed",
                            json!({ "kind": kind, "message": e.to_string() }),
                        );
                    } else {
                        let _ = self
                            .db
                            .call(move |conn| {
                                conn.execute(
                                    "UPDATE pending_ops SET attempts = attempts + 1 WHERE id = ?1",
                                    rusqlite::params![op_id],
                                )
                                .map(|_| ())
                            })
                            .await;
                        break;
                    }
                }
            }
        }
    }

    async fn finish_op(&self, op_id: i64, success: bool) -> Result<()> {
        self.db
            .call(move |conn| {
                if success {
                    conn.execute(
                        "DELETE FROM pending_ops WHERE id = ?1",
                        rusqlite::params![op_id],
                    )?;
                } else {
                    conn.execute(
                        "UPDATE pending_ops SET state = 'failed' WHERE id = ?1",
                        rusqlite::params![op_id],
                    )?;
                }
                Ok(())
            })
            .await
    }

    async fn execute_op(&mut self, kind: &str, payload: &serde_json::Value) -> Result<()> {
        match kind {
            "send" => self.execute_send(payload).await,
            "rsvp" => self.execute_rsvp(payload).await,
            "unsubscribe" => self.execute_unsubscribe(payload).await,
            // Everything else is an instruction for a server-side mailbox —
            // flags, folders, server drafts. POP3 has none: the command layer
            // has already applied the change locally, and that is all of it.
            _ => Ok(()),
        }
    }

    async fn smtp_send(&self, raw: &[u8]) -> Result<()> {
        let creds = imap_client::Credentials::Password(self.password()?);
        smtp::send(&self.account, &creds, raw).await
    }

    /// File a sent message in the local Sent folder, already read.
    async fn file_sent(&self, raw: Vec<u8>) {
        let account_id = self.account.id.clone();
        let data_dir = self.data_dir.clone();
        let filed = self
            .db
            .call(move |conn| {
                local_store::ensure_folders(conn, &account_id)?;
                let Some(sent) = local_store::folder_by_role(conn, &account_id, "sent")? else {
                    return Ok(None);
                };
                Ok(
                    local_store::store_raw(conn, &data_dir, &account_id, sent, &raw, true)
                        .ok()
                        .flatten()
                        .map(|_| sent),
                )
            })
            .await;
        match filed {
            Ok(Some(sent)) => {
                let _ = self.app.emit("mail:updated", json!({ "folderId": sent }));
            }
            // The message did go out; not having a local copy is a loss worth
            // a log line, not a reason to send it again.
            Ok(None) => tracing::warn!("sent message was not filed locally"),
            Err(e) => tracing::warn!(error = %e, "cannot file sent message locally"),
        }
    }

    async fn execute_send(&mut self, payload: &serde_json::Value) -> Result<()> {
        let Some(draft_id) = payload["draftId"].as_i64() else {
            return Ok(());
        };
        let draft = self
            .db
            .call(move |conn| crate::db::drafts::get(conn, draft_id))
            .await?;
        let Some(draft) = draft else {
            return Ok(()); // already sent or discarded
        };
        let refs = self.outgoing_refs(draft.reply_to_message_id).await?;
        let attachments = self
            .db
            .call(move |conn| crate::db::draft_attachments::load_for_send(conn, draft_id))
            .await?;
        let raw = smtp::build_message(&self.account, &draft, &refs, &attachments, None, false)?;
        self.smtp_send(&raw).await?;
        self.file_sent(raw).await;

        self.db
            .call(move |conn| crate::db::drafts::delete(conn, draft_id))
            .await?;
        let _ = self.app.emit("drafts:updated", json!({}));
        let _ = self.app.emit("mail:sent", json!({ "draftId": draft_id }));
        Ok(())
    }

    /// In-Reply-To/References for an outgoing reply, from its local parent.
    async fn outgoing_refs(&self, reply_to_message_id: Option<i64>) -> Result<smtp::OutgoingRefs> {
        let Some(parent_id) = reply_to_message_id else {
            return Ok(smtp::OutgoingRefs {
                in_reply_to: None,
                references: Vec::new(),
            });
        };
        self.db
            .call(move |conn| {
                use rusqlite::OptionalExtension;
                let row: Option<(Option<String>, Option<String>)> = conn
                    .query_row(
                        "SELECT message_id, references_ids FROM messages WHERE id = ?1",
                        rusqlite::params![parent_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let (msgid, refs_json) = row.unwrap_or((None, None));
                let mut references: Vec<String> = refs_json
                    .and_then(|j| serde_json::from_str::<Vec<String>>(&j).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| format!("<{r}>"))
                    .collect();
                let in_reply_to = msgid.map(|m| format!("<{m}>"));
                if let Some(irt) = &in_reply_to {
                    references.push(irt.clone());
                }
                Ok(smtp::OutgoingRefs {
                    in_reply_to,
                    references,
                })
            })
            .await
    }

    async fn execute_rsvp(&mut self, payload: &serde_json::Value) -> Result<()> {
        let to = payload["to"].as_str().unwrap_or_default();
        let subject = payload["subject"].as_str().unwrap_or_default();
        let text_body = payload["textBody"].as_str().unwrap_or_default();
        let ics = payload["ics"].as_str().unwrap_or_default();
        if to.is_empty() || ics.is_empty() {
            return Ok(());
        }
        let raw = smtp::build_calendar_reply(&self.account, to, subject, text_body, ics)?;
        self.smtp_send(&raw).await?;
        self.file_sent(raw).await;
        Ok(())
    }

    async fn execute_unsubscribe(&mut self, payload: &serde_json::Value) -> Result<()> {
        match payload["method"].as_str() {
            Some("post") => {
                let url = payload["url"].as_str().unwrap_or_default();
                if url.is_empty() {
                    return Ok(());
                }
                // Same SSRF boundary as the IMAP engine: the URL comes from the
                // sender, so https only, public addresses only, pinned.
                let (target, addrs) = crate::net::vet_public_url(url, true, "unsubscribe").await?;
                let host = target.host_str().unwrap_or_default().to_string();
                let client = crate::net::http_builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .timeout(std::time::Duration::from_secs(30))
                    .resolve_to_addrs(&host, &addrs)
                    .build()
                    .map_err(|e| SkimError::other("unsubscribe", e.to_string()))?;
                let resp = client
                    .post(target)
                    .header("Content-Type", "application/x-www-form-urlencoded")
                    .body("List-Unsubscribe=One-Click")
                    .send()
                    .await
                    .map_err(|e| SkimError::other("unsubscribe", e.to_string()))?;
                if !resp.status().is_success() {
                    return Err(SkimError::other(
                        "unsubscribe",
                        format!("list server returned {}", resp.status()),
                    ));
                }
                Ok(())
            }
            Some("mail") => {
                let to = payload["to"].as_str().unwrap_or_default();
                let subject = payload["subject"].as_str().unwrap_or("unsubscribe");
                if to.is_empty() {
                    return Ok(());
                }
                let raw = smtp::build_unsubscribe_mail(&self.account, to, subject)?;
                self.smtp_send(&raw).await
            }
            _ => Ok(()),
        }
    }
}

/// Start the engine for a POP3 account. Mirrors `sync::spawn`.
pub fn spawn(app: AppHandle, db: Db, account: Account, data_dir: PathBuf) -> SyncHandle {
    let (tx, mut rx) = mpsc::unbounded_channel::<SyncCommand>();
    let (fetch_tx, mut fetch_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let handle = SyncHandle {
        tx: tx.clone(),
        fetch_tx,
    };

    // Bodies are stored with the message at download time, so there is never
    // anything to fetch on a click — just answer, so the caller doesn't wait.
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

    tauri::async_runtime::spawn(async move {
        let mut engine = Engine {
            app,
            db,
            account,
            data_dir,
        };
        // Folders first, so a brand-new account has a sidebar before (and even
        // without) its first successful login.
        let account_id = engine.account.id.clone();
        let _ = engine
            .db
            .call(move |conn| {
                ensure_schema(conn)?;
                local_store::ensure_folders(conn, &account_id)
            })
            .await;
        let _ = engine.app.emit("folders:updated", json!({}));

        let mut poll = tokio::time::interval(POLL_INTERVAL);
        poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            let sync = tokio::select! {
                cmd = rx.recv() => match cmd {
                    None | Some(SyncCommand::Stop) => break,
                    Some(SyncCommand::SyncAll) | Some(SyncCommand::SyncInbox) => true,
                    Some(SyncCommand::RunOps) => {
                        engine.drain_ops().await;
                        false
                    }
                    Some(_) => false,
                },
                _ = poll.tick() => true,
            };
            if sync {
                engine.drain_ops().await;
                if engine.run_sync().await {
                    // More of the first download is waiting. Queued behind
                    // whatever else arrived meanwhile, so a send or a stop
                    // is handled between two batches.
                    let _ = tx.send(SyncCommand::SyncAll);
                }
            }
        }
    });

    handle
}

#[cfg(test)]
mod tests {
    use super::*;
    use pop3_client::Listing;

    fn listing(entries: &[(u32, &str)]) -> Vec<Listing> {
        entries
            .iter()
            .map(|(number, uidl)| Listing {
                number: *number,
                uidl: (*uidl).to_string(),
            })
            .collect()
    }

    fn seen(ids: &[&str]) -> HashSet<String> {
        ids.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn only_unseen_mail_is_downloaded() {
        let (batch, more) = plan_batch(
            &listing(&[(1, "a"), (2, "b"), (3, "c")]),
            &seen(&["a", "b"]),
            10,
        );
        assert_eq!(batch, listing(&[(3, "c")]));
        assert!(!more);
    }

    #[test]
    fn the_newest_mail_comes_first() {
        let (batch, _) = plan_batch(&listing(&[(1, "a"), (2, "b"), (3, "c")]), &seen(&[]), 10);
        let order: Vec<u32> = batch.iter().map(|l| l.number).collect();
        assert_eq!(order, [3, 2, 1]);
    }

    #[test]
    fn a_deep_maildrop_is_taken_a_batch_at_a_time() {
        let (batch, more) = plan_batch(&listing(&[(1, "a"), (2, "b"), (3, "c")]), &seen(&[]), 2);
        assert_eq!(batch.len(), 2);
        assert!(more, "the rest waits for the next pass");
        assert_eq!(batch[0].number, 3);
    }

    #[test]
    fn nothing_new_means_nothing_to_do() {
        let (batch, more) = plan_batch(&listing(&[(1, "a")]), &seen(&["a"]), 10);
        assert!(batch.is_empty());
        assert!(!more);
    }

    #[test]
    fn ids_the_server_dropped_are_forgotten() {
        let mut stale = stale_ids(&listing(&[(1, "b")]), &seen(&["a", "b", "c"]));
        stale.sort();
        assert_eq!(stale, ["a", "c"]);
    }

    #[test]
    fn the_seen_table_goes_with_its_account() {
        let db = Db::open_in_memory().unwrap();
        db.with(|conn| {
            ensure_schema(conn)?;
            ensure_schema(conn)?; // idempotent
            conn.execute(
                "INSERT INTO accounts (id, email, provider, imap_host, smtp_host, created_at)
                 VALUES ('acc', 'me@example.com', 'pop3', 'pop.example.com', 'smtp.example.com', 0)",
                [],
            )?;
            conn.execute(
                "INSERT INTO pop3_seen (account_id, uidl) VALUES ('acc', 'x')",
                [],
            )?;
            db_accounts::delete(conn, "acc")?;
            let left: i64 = conn.query_row("SELECT count(*) FROM pop3_seen", [], |r| r.get(0))?;
            assert_eq!(left, 0);
            Ok(())
        })
        .unwrap();
    }
}
