pub mod autoconfig;
pub mod dns;
pub mod ics;
pub mod imap_client;
pub mod lang;
pub mod local_store;
pub mod oauth;
pub mod parse;
pub mod pop3;
pub mod pop3_client;
pub mod sanitize;
pub mod smtp;
pub mod suspicion;
pub mod sync;
pub mod threading;
pub mod translate;

/// Start the engine that matches the account's kind: POP3 accounts get the
/// download-only engine, everything else the IMAP sync engine. Fork addition —
/// the one place that decides, so callers never look at the provider.
pub fn spawn_engine(
    app: tauri::AppHandle,
    db: crate::db::Db,
    account: crate::db::models::Account,
    data_dir: std::path::PathBuf,
) -> sync::SyncHandle {
    if account.provider == local_store::POP3 {
        pop3::spawn(app, db, account, data_dir)
    } else {
        sync::spawn(app, db, account, data_dir)
    }
}

#[cfg(test)]
#[path = "threading_tests.rs"]
mod threading_tests;
