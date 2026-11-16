//! IPC surface of the address book (fork addition, see `crate::contacts`).

use crate::commands::compose::AddressSuggestion;
use crate::contacts::{self, Contact, Group, ImportReport};
use crate::db::queries;
use crate::error::{Result, SkimError};
use crate::state::AppState;
use tauri::{AppHandle, State};
use tauri_plugin_dialog::DialogExt;

/// Largest file accepted for a contacts import.
const MAX_IMPORT_BYTES: u64 = 20 * 1024 * 1024;

/// Suggestions shown under a recipient field.
const MAX_SUGGESTIONS: usize = 8;

/// Put the address book in front of the mailbox's own suggestions: a matching
/// group, then saved contacts, then whoever the mailbox knows — without
/// offering the same address twice. Called from `compose::suggest_addresses`.
pub fn with_address_book(
    conn: &rusqlite::Connection,
    like: &str,
    from_mailbox: Vec<AddressSuggestion>,
) -> rusqlite::Result<Vec<AddressSuggestion>> {
    let mut out: Vec<AddressSuggestion> = contacts::suggest(conn, like, MAX_SUGGESTIONS)?
        .into_iter()
        .map(|s| AddressSuggestion {
            name: s.name,
            addr: s.addr,
        })
        .collect();
    for s in from_mailbox {
        if !out
            .iter()
            .any(|known| known.addr.eq_ignore_ascii_case(&s.addr))
        {
            out.push(s);
        }
    }
    out.truncate(MAX_SUGGESTIONS);
    Ok(out)
}

#[tauri::command]
pub async fn list_contacts(state: State<'_, AppState>) -> Result<Vec<Contact>> {
    state.db.call(|conn| contacts::list(conn)).await
}

/// Add or edit a contact. A new contact whose address is already in the book
/// updates that entry instead of making a second one.
#[tauri::command]
pub async fn save_contact(state: State<'_, AppState>, contact: Contact) -> Result<Contact> {
    if !contacts::valid_email(&contact.email) {
        return Err(SkimError::other(
            "contact_email",
            "that is not an e-mail address",
        ));
    }
    let id = contact.id;
    let email = contact.email.trim().to_string();
    state
        .db
        .call(move |conn| {
            // Editing a contact onto another contact's address would break
            // the one-address-one-contact rule; say so instead of failing on
            // the constraint.
            let taken: bool = conn
                .prepare_cached("SELECT 1 FROM pcg_contacts WHERE email = ?1 AND id <> ?2")?
                .exists(rusqlite::params![email, id])?;
            if id != 0 && taken {
                return Ok(Err(SkimError::other(
                    "contact_exists",
                    "another contact already has this address",
                )));
            }
            contacts::save(conn, &contact).map(|(stored, _)| Ok(stored))
        })
        .await?
}

#[tauri::command]
pub async fn delete_contacts(state: State<'_, AppState>, ids: Vec<i64>) -> Result<()> {
    state
        .db
        .call(move |conn| contacts::delete(conn, &ids))
        .await
}

#[tauri::command]
pub async fn list_contact_groups(state: State<'_, AppState>) -> Result<Vec<Group>> {
    state.db.call(|conn| contacts::list_groups(conn)).await
}

/// Create (`id` 0) or rename a group and set its members.
#[tauri::command]
pub async fn save_contact_group(
    state: State<'_, AppState>,
    id: i64,
    name: String,
    member_ids: Vec<i64>,
) -> Result<Group> {
    if name.trim().is_empty() {
        return Err(SkimError::other("group_name", "the group needs a name"));
    }
    state
        .db
        .call(move |conn| {
            let taken: bool = conn
                .prepare_cached("SELECT 1 FROM pcg_contact_groups WHERE name = ?1 AND id <> ?2")?
                .exists(rusqlite::params![name.trim(), id])?;
            if taken {
                return Ok(Err(SkimError::other(
                    "group_exists",
                    "a group with this name already exists",
                )));
            }
            contacts::save_group(conn, id, &name, &member_ids).map(Ok)
        })
        .await?
}

#[tauri::command]
pub async fn delete_contact_group(state: State<'_, AppState>, id: i64) -> Result<()> {
    state
        .db
        .call(move |conn| contacts::delete_group(conn, id))
        .await
}

/// Ask for a vCard or CSV file and merge its contacts into the book. `None`
/// when the user cancelled.
#[tauri::command]
pub async fn import_contacts(
    app: AppHandle,
    state: State<'_, AppState>,
) -> Result<Option<ImportReport>> {
    let dialog = app
        .dialog()
        .file()
        .add_filter("vCard, CSV", &["vcf", "csv", "txt"]);
    let picked = tokio::task::spawn_blocking(move || dialog.blocking_pick_file())
        .await
        .map_err(|e| SkimError::other("internal", e.to_string()))?;
    let Some(picked) = picked else {
        return Ok(None);
    };
    let path = picked
        .into_path()
        .map_err(|e| SkimError::other("io", e.to_string()))?;
    // An address book of tens of thousands of people is a few megabytes; a
    // file far beyond that is not one, and would only tie the app up.
    if tokio::fs::metadata(&path).await?.len() > MAX_IMPORT_BYTES {
        return Err(SkimError::other(
            "contacts_too_large",
            "this file is too large to be an address book",
        ));
    }
    let bytes = tokio::fs::read(&path).await?;

    // A file that is not Unicode says nothing about its encoding; the UI
    // language is the best hint for which Windows wrote it.
    let locale = state
        .db
        .call(|conn| queries::get_setting(conn, "locale"))
        .await?
        .unwrap_or_default();
    // Parsed off the database thread: sync and the op queue keep going.
    let (entries, skipped) = tokio::task::spawn_blocking(move || {
        let text = contacts::decode_file(&bytes, contacts::legacy_encoding(&locale));
        contacts::parse_file(&text)
    })
    .await
    .map_err(|e| SkimError::other("internal", e.to_string()))?;
    let report = state
        .db
        .call(move |conn| contacts::import(conn, &entries, skipped))
        .await?;
    if report.added + report.updated + report.skipped == 0 {
        return Err(SkimError::other(
            "contacts_import",
            "no contacts were found in this file",
        ));
    }
    Ok(Some(report))
}

/// Ask where to save, and write the whole book as a vCard file. Returns the
/// path, or `None` when the user cancelled.
#[tauri::command]
pub async fn export_contacts(app: AppHandle, state: State<'_, AppState>) -> Result<Option<String>> {
    let dialog = app
        .dialog()
        .file()
        .add_filter("vCard", &["vcf"])
        .set_file_name("contacts.vcf");
    let picked = tokio::task::spawn_blocking(move || dialog.blocking_save_file())
        .await
        .map_err(|e| SkimError::other("internal", e.to_string()))?;
    let Some(dest) = picked else {
        return Ok(None);
    };
    let dest = dest
        .into_path()
        .map_err(|e| SkimError::other("io", e.to_string()))?;
    let text = state
        .db
        .call(|conn| {
            Ok(contacts::export_vcards(
                &contacts::list(conn)?,
                &contacts::list_groups(conn)?,
            ))
        })
        .await?;
    tokio::fs::write(&dest, text).await?;
    Ok(Some(dest.to_string_lossy().into_owned()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn mailbox(addr: &str) -> AddressSuggestion {
        AddressSuggestion {
            name: None,
            addr: addr.into(),
        }
    }

    #[test]
    fn the_address_book_comes_first_and_nothing_is_offered_twice() {
        let db = Db::open_in_memory().unwrap();
        db.with(|conn| {
            contacts::ensure_schema(conn)?;
            contacts::save(
                conn,
                &Contact {
                    name: "Ann".into(),
                    email: "ann@example.com".into(),
                    ..Default::default()
                },
            )?;
            let merged = with_address_book(
                conn,
                "%example%",
                vec![mailbox("ANN@example.com"), mailbox("zed@example.com")],
            )?;
            let addrs: Vec<&str> = merged.iter().map(|s| s.addr.as_str()).collect();
            assert_eq!(addrs, ["ann@example.com", "zed@example.com"]);
            assert_eq!(merged[0].name.as_deref(), Some("Ann"));
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_list_stays_short() {
        let db = Db::open_in_memory().unwrap();
        db.with(|conn| {
            contacts::ensure_schema(conn)?;
            let many = (0..20)
                .map(|i| mailbox(&format!("p{i}@example.com")))
                .collect();
            assert_eq!(with_address_book(conn, "%p%", many)?.len(), MAX_SUGGESTIONS);
            Ok(())
        })
        .unwrap();
    }
}
