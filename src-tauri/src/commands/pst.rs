//! IPC surface of the PST import (fork addition, see `mail::pst_import`).

use crate::db::accounts as db_accounts;
use crate::db::models::Account;
use crate::error::{Result, SkimError};
use crate::mail::{local_store, pst_import};
use crate::state::AppState;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

/// The archive account for a PST file: the one made for this very file on an
/// earlier import (matched by the file's own identity, not its name), or a new
/// one named after the file.
fn archive_account(
    conn: &rusqlite::Connection,
    key: &str,
    file_name: &str,
    stem: &str,
) -> rusqlite::Result<(Account, bool)> {
    use rusqlite::OptionalExtension;
    let existing: Option<String> = conn
        .query_row(
            "SELECT id FROM accounts WHERE provider = ?1 AND imap_user = ?2",
            rusqlite::params![local_store::ARCHIVE, key],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(account) = existing.and_then(|id| db_accounts::get(conn, &id).transpose()) {
        return Ok((account?, false));
    }
    // `email` is the account's unique label; two different files may share a
    // name, so later ones are numbered.
    let mut email = file_name.to_string();
    let mut n = 2;
    while conn
        .prepare_cached("SELECT 1 FROM accounts WHERE email = ?1 COLLATE NOCASE")?
        .exists(rusqlite::params![email])?
    {
        email = format!("{file_name} ({n})");
        n += 1;
    }
    let account = Account {
        id: uuid::Uuid::new_v4().to_string(),
        email,
        imap_user: Some(key.to_string()),
        display_name: Some(stem.to_string()),
        provider: local_store::ARCHIVE.into(),
        imap_host: file_name.to_string(),
        imap_port: 0,
        smtp_host: String::new(),
        smtp_port: 0,
        smtp_security: "none".into(),
        auth_kind: "none".into(),
        signature: None,
    };
    db_accounts::insert(conn, &account)?;
    Ok((account, true))
}

/// Ask for a `.pst` file and import it into a local archive. Returns the
/// archive's account right away — the import itself runs in the background
/// and reports through `import:progress` — or `None` if the user cancelled.
#[tauri::command]
pub async fn import_pst(app: AppHandle, state: State<'_, AppState>) -> Result<Option<Account>> {
    let dialog = app
        .dialog()
        .file()
        .add_filter("Outlook data file", &["pst"]);
    let picked = tokio::task::spawn_blocking(move || dialog.blocking_pick_file())
        .await
        .map_err(|e| SkimError::other("internal", e.to_string()))?;
    let Some(picked) = picked else {
        return Ok(None);
    };
    let path = picked
        .into_path()
        .map_err(|e| SkimError::other("io", e.to_string()))?;

    // Opening a large file walks its index; keep that off the async runtime.
    let probe = path.clone();
    let identity = tokio::task::spawn_blocking(move || pst_import::identify(&probe))
        .await
        .map_err(|e| SkimError::other("internal", e.to_string()))??;

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "archive.pst".into());
    let stem = path
        .file_stem()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| file_name.clone());
    let (account, created) = state
        .db
        .call(move |conn| archive_account(conn, &identity.key, &file_name, &stem))
        .await?;

    if created {
        let handle = crate::mail::spawn_engine(
            app.clone(),
            state.db.clone(),
            account.clone(),
            state.data_dir.clone(),
        );
        state
            .engines
            .lock()
            .await
            .insert(account.id.clone(), handle);
    }
    pst_import::launch(&app, &state.db, &state.data_dir, &account.id, path);
    Ok(Some(account))
}

/// Stop a running import. What is already imported stays.
#[tauri::command]
pub fn cancel_pst_import(account_id: String) {
    pst_import::cancel(&account_id);
}

/// Imports in progress, for a settings view opened mid-import.
#[tauri::command]
pub fn pst_import_status() -> Vec<pst_import::Progress> {
    pst_import::status()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn the_same_file_finds_its_archive_again() {
        let db = Db::open_in_memory().unwrap();
        db.with(|conn| {
            let (first, created) = archive_account(conn, "k1", "old.pst", "old")?;
            assert!(created);
            assert_eq!(first.provider, "local");
            assert_eq!(first.display_name.as_deref(), Some("old"));
            // Moved and renamed, but the same file by its identity.
            let (again, created) = archive_account(conn, "k1", "renamed.pst", "renamed")?;
            assert!(!created);
            assert_eq!(again.id, first.id);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn two_different_files_with_one_name_get_two_archives() {
        let db = Db::open_in_memory().unwrap();
        db.with(|conn| {
            let (a, _) = archive_account(conn, "k1", "backup.pst", "backup")?;
            let (b, created) = archive_account(conn, "k2", "backup.pst", "backup")?;
            assert!(created);
            assert_ne!(a.id, b.id);
            assert_eq!(b.email, "backup.pst (2)");
            Ok(())
        })
        .unwrap();
    }
}
