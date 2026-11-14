//! Outlook PST import.
//!
//! Fork addition. Reads an Outlook data file (`.pst`, both the Unicode format
//! of Outlook 2003+ and the older ANSI one) and writes its mail into a local
//! archive account through [`local_store::store_message`] — the same path POP3
//! uses, so everything downstream (reading pane, search, threading, AI) works
//! on imported mail unchanged.
//!
//! A PST does not hold RFC 822 messages; it holds MAPI property bags. Rather
//! than synthesize MIME only to parse it again, a message is converted straight
//! into the two structures the store wants ([`NewMessage`] + [`ParsedBody`]).
//! The original internet headers, when Outlook kept them, are still the best
//! source for threading ids and addresses, so they are parsed first and the
//! MAPI properties fill in what they lack.
//!
//! The file is opened read-only and never modified.
//!
//! Large files: a PST can run to tens of gigabytes. Messages are imported one
//! at a time, each recorded by its node id in `pst_imported` once it is safely
//! stored, so an import that is interrupted — cancelled, app closed, machine
//! asleep — continues from where it stopped instead of starting over.
//!
//! Not imported: calendar items, contacts, tasks and notes (Skim is a mail
//! client); messages attached to messages; bodies that exist only as
//! compressed RTF (their plain-text version is used when Outlook stored one).

use crate::db::models::{Address, NewMessage};
use crate::db::Db;
use crate::error::{Result, SkimError};
use crate::mail::parse::{self, ParsedAttachment, ParsedBody};
use crate::mail::{local_store, tnef};
use outlook_pst::ltp::prop_context::PropertyValue;
use outlook_pst::messaging::attachment::{
    AnsiAttachment, Attachment, AttachmentData, UnicodeAttachment,
};
use outlook_pst::messaging::folder::Folder;
use outlook_pst::messaging::message::{AnsiMessage, Message, UnicodeMessage};
use outlook_pst::messaging::store::{AnsiStore, Store, UnicodeStore};
use outlook_pst::ndb::node_id::NodeId;
use outlook_pst::{AnsiPstFile, UnicodePstFile};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tauri::{AppHandle, Emitter};

// MAPI property ids used below.
const PR_SUBJECT: u16 = 0x0037;
const PR_CLIENT_SUBMIT_TIME: u16 = 0x0039;
const PR_SENT_REPRESENTING_NAME: u16 = 0x0042;
const PR_SENT_REPRESENTING_EMAIL: u16 = 0x0065;
const PR_TRANSPORT_HEADERS: u16 = 0x007D;
const PR_RECIPIENT_TYPE: u16 = 0x0C15;
const PR_SENDER_NAME: u16 = 0x0C1A;
const PR_SENDER_EMAIL: u16 = 0x0C1F;
const PR_MESSAGE_DELIVERY_TIME: u16 = 0x0E06;
const PR_MESSAGE_FLAGS: u16 = 0x0E07;
const PR_MESSAGE_SIZE: u16 = 0x0E08;
const PR_BODY: u16 = 0x1000;
const PR_HTML: u16 = 0x1013;
const PR_INTERNET_MESSAGE_ID: u16 = 0x1035;
const PR_INTERNET_REFERENCES: u16 = 0x1039;
const PR_IN_REPLY_TO_ID: u16 = 0x1042;
const PR_FLAG_STATUS: u16 = 0x1090;
const PR_DISPLAY_NAME: u16 = 0x3001;
const PR_EMAIL_ADDRESS: u16 = 0x3003;
const PR_CREATION_TIME: u16 = 0x3007;
const PR_CONTAINER_CLASS: u16 = 0x3613;
const PR_ATTACH_FILENAME: u16 = 0x3704;
const PR_ATTACH_LONG_FILENAME: u16 = 0x3707;
const PR_ATTACH_MIME_TAG: u16 = 0x370E;
const PR_ATTACH_CONTENT_ID: u16 = 0x3712;
const PR_SMTP_ADDRESS: u16 = 0x39FE;
const PR_INTERNET_CPID: u16 = 0x3FDE;
const PR_MESSAGE_CODEPAGE: u16 = 0x3FFD;
const PR_SENDER_SMTP: u16 = 0x5D01;
const PR_SENT_REPRESENTING_SMTP: u16 = 0x5D02;

const MSGFLAG_READ: i32 = 0x1;
const FOLLOWUP_FLAGGED: i32 = 2;
const MAPI_TO: i32 = 1;
const MAPI_CC: i32 = 2;

/// Seconds between the FILETIME epoch (1601) and the Unix epoch (1970).
const FILETIME_UNIX_OFFSET: i64 = 11_644_473_600;

/// How often progress is reported, in messages.
const REPORT_EVERY: u64 = 25;
/// Consecutive write failures after which the import gives up.
const MAX_IO_FAILURES: u32 = 20;

fn pst_err(e: impl std::fmt::Display) -> SkimError {
    SkimError::other("pst", e.to_string())
}

/// A state of the import, as shown to the user.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Running,
    Done,
    Cancelled,
    Error,
}

/// Running totals of one import.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Progress {
    pub account_id: String,
    pub phase: Phase,
    /// Mail items the file holds in its mail folders.
    pub total: u64,
    /// Items dealt with so far: imported now, imported earlier, or skipped.
    pub done: u64,
    /// Messages written to the database by this run.
    pub imported: u64,
    /// Items that could not be read or stored.
    pub failed: u64,
    /// Set when `phase` is `Error`.
    pub message: Option<String>,
}

// ---- reading the file ------------------------------------------------------

/// The two on-disk formats. The library's attachment API is typed by format,
/// so the store is kept in its concrete form rather than as `dyn Store`.
enum Source {
    Unicode(Rc<UnicodeStore>),
    Ansi(Rc<AnsiStore>),
}

impl Source {
    /// Open read-only. `open()` in the library also tries for write access;
    /// reading from a plain `File` cannot touch the user's archive.
    fn open(path: &Path) -> Result<Self> {
        let file = || std::fs::File::open(path).map_err(pst_err);
        if let Ok(pst) = UnicodePstFile::read_from(Box::new(file()?)) {
            return Ok(Source::Unicode(
                UnicodeStore::read(Rc::new(pst)).map_err(pst_err)?,
            ));
        }
        let pst = AnsiPstFile::read_from(Box::new(file()?)).map_err(|e| {
            SkimError::other("pst", format!("this is not an Outlook data file: {e}"))
        })?;
        Ok(Source::Ansi(
            AnsiStore::read(Rc::new(pst)).map_err(pst_err)?,
        ))
    }

    fn store(&self) -> &dyn Store {
        match self {
            Source::Unicode(s) => s.as_ref(),
            Source::Ansi(s) => s.as_ref(),
        }
    }

    /// The file's own identity: a GUID Outlook assigns when it creates the
    /// store. Lets a second import of the same file find its archive again.
    fn record_key(&self) -> Result<String> {
        let key = self.store().properties().record_key().map_err(pst_err)?;
        Ok(key
            .record_key()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect())
    }

    fn folder(&self, node: NodeId) -> std::io::Result<Rc<dyn Folder>> {
        let entry_id = self.store().properties().make_entry_id(node)?;
        self.store().open_folder(&entry_id)
    }

    /// A message with all its properties, and the attachments that could be
    /// read. An unreadable attachment is dropped; the message still imports.
    fn message(&self, node: NodeId) -> std::io::Result<OpenedMessage> {
        let entry_id = self.store().properties().make_entry_id(node)?;
        macro_rules! read {
            ($store:expr, $message:ty, $attachment:ty) => {{
                let message = <$message>::read($store.clone(), &entry_id, None)?;
                let mut attachments: Vec<Rc<dyn Attachment>> = Vec::new();
                if let Some(table) = message.attachment_table() {
                    for row in table.rows_matrix() {
                        let node = NodeId::from(u32::from(row.id()));
                        if let Ok(a) = <$attachment>::read(message.clone(), node, None) {
                            attachments.push(a);
                        }
                    }
                }
                let message: Rc<dyn Message> = message;
                Ok((message, attachments))
            }};
        }
        match self {
            Source::Unicode(store) => read!(store, UnicodeMessage, UnicodeAttachment),
            Source::Ansi(store) => read!(store, AnsiMessage, AnsiAttachment),
        }
    }
}

/// A message and the attachments of it that could be read.
type OpenedMessage = (Rc<dyn Message>, Vec<Rc<dyn Attachment>>);

/// A mail folder found in the file.
struct PstFolder {
    /// Names from the top of the mailbox down to this folder.
    path: Vec<String>,
    folder: Rc<dyn Folder>,
}

fn text_of(value: Option<&PropertyValue>, codepage: u32) -> Option<String> {
    let text = match value? {
        PropertyValue::String8(v) => tnef::decode_string8(v.buffer(), codepage),
        PropertyValue::Unicode(v) => v.to_string(),
        _ => return None,
    };
    let text = text.trim_end_matches('\0').to_string();
    Some(text).filter(|t| !t.trim().is_empty())
}

fn int_of(value: Option<&PropertyValue>) -> Option<i32> {
    match value? {
        PropertyValue::Integer32(v) => Some(*v),
        PropertyValue::Integer16(v) => Some(i32::from(*v)),
        _ => None,
    }
}

/// FILETIME (100 ns ticks since 1601) to Unix seconds; `None` when unset.
fn unix_time(value: Option<&PropertyValue>) -> Option<i64> {
    match value? {
        PropertyValue::Time(ticks) if *ticks > 0 => {
            Some(ticks / 10_000_000 - FILETIME_UNIX_OFFSET).filter(|t| *t > 0)
        }
        _ => None,
    }
}

/// Is this a folder that holds mail? Calendar, contacts, tasks, journal and
/// notes folders say so in their container class; mail folders say `IPF.Note`
/// or nothing at all.
fn is_mail_folder(class: Option<&str>) -> bool {
    match class {
        None => true,
        Some(c) => {
            let c = c.to_ascii_lowercase();
            c.is_empty() || c == "ipf.note" || c.starts_with("ipf.note.")
        }
    }
}

/// Is this item mail? Notes, meeting requests, posts and delivery reports are;
/// contacts, appointments, tasks, sticky notes and journal entries are not.
fn is_mail_item(class: &str) -> bool {
    let c = class.to_ascii_lowercase();
    ["ipm.note", "ipm.schedule.", "ipm.post", "report."]
        .iter()
        .any(|p| c.starts_with(p))
        || c == "ipm"
}

/// Walk the mailbox tree, collecting mail folders in display order.
fn collect_folders(source: &Source) -> Result<Vec<PstFolder>> {
    let root_id = source
        .store()
        .properties()
        .ipm_sub_tree_entry_id()
        .map_err(pst_err)?;
    let root = source.store().open_folder(&root_id).map_err(pst_err)?;
    let mut out = Vec::new();
    walk(source, &root, &[], &mut out, 0);
    Ok(out)
}

fn walk(
    source: &Source,
    parent: &Rc<dyn Folder>,
    path: &[String],
    out: &mut Vec<PstFolder>,
    depth: u8,
) {
    // A folder tree this deep is a corrupt file looping on itself.
    if depth > 32 {
        return;
    }
    let Some(table) = parent.hierarchy_table() else {
        return;
    };
    let mut children: Vec<(String, Rc<dyn Folder>)> = table
        .rows_matrix()
        .filter_map(|row| source.folder(NodeId::from(u32::from(row.id()))).ok())
        .map(|f| {
            let name = f.properties().display_name().unwrap_or_default();
            // "/" is the path separator in the sidebar.
            let name = name.trim().replace('/', "-");
            (name, f)
        })
        .filter(|(name, _)| !name.is_empty())
        .collect();
    children.sort_by_key(|(name, _)| name.to_lowercase());
    for (name, folder) in children {
        let class = text_of(folder.properties().get(PR_CONTAINER_CLASS), 1252);
        let mut child_path = path.to_vec();
        child_path.push(name);
        if is_mail_folder(class.as_deref()) {
            out.push(PstFolder {
                path: child_path.clone(),
                folder: folder.clone(),
            });
        }
        // Mail folders can sit under a non-mail parent, so always descend.
        walk(source, &folder, &child_path, out, depth + 1);
    }
}

/// The special-use role of a top-level folder, by the names Outlook gives it
/// in English, Chinese and Indonesian.
fn role_for(path: &[String]) -> Option<&'static str> {
    let [name] = path else { return None };
    let role = match name.to_lowercase().as_str() {
        "inbox" | "收件匣" | "收件箱" | "kotak masuk" => "inbox",
        "sent items" | "sent" | "sent mail" | "寄件備份" | "已傳送的郵件" | "已发送邮件"
        | "item terkirim" => "sent",
        "deleted items" | "trash" | "刪除的郵件" | "已删除邮件" | "item terhapus" => {
            "trash"
        }
        "junk e-mail" | "junk email" | "junk" | "垃圾郵件" | "垃圾邮件" | "email sampah" => {
            "junk"
        }
        "archive" | "封存" | "存档" | "arsip" => "archive",
        _ => return None,
    };
    Some(role)
}

// ---- converting one message -------------------------------------------------

fn addr_candidate<'a>(candidates: impl IntoIterator<Item = Option<&'a String>>) -> Option<String> {
    let all: Vec<&String> = candidates.into_iter().flatten().collect();
    // An SMTP address when there is one; an Exchange directory name
    // ("/O=ORG/CN=…") is better than nothing but not something to reply to.
    all.iter()
        .find(|a| a.contains('@'))
        .or(all.first())
        .map(|a| a.trim().to_string())
}

fn recipients(message: &dyn Message, codepage: u32) -> (Vec<Address>, Vec<Address>) {
    let (mut to, mut cc) = (Vec::new(), Vec::new());
    let Some(table) = message.recipient_table() else {
        return (to, cc);
    };
    let context = table.context();
    for row in table.rows_matrix() {
        let Ok(columns) = row.columns(context) else {
            continue;
        };
        let mut props: HashMap<u16, PropertyValue> = HashMap::new();
        for (column, value) in context.columns().iter().zip(columns) {
            let wanted = matches!(
                column.prop_id(),
                PR_RECIPIENT_TYPE | PR_DISPLAY_NAME | PR_EMAIL_ADDRESS | PR_SMTP_ADDRESS
            );
            if let (true, Some(value)) = (wanted, value) {
                if let Ok(v) = table.read_column(&value, column.prop_type()) {
                    props.insert(column.prop_id(), v);
                }
            }
        }
        let name = text_of(props.get(&PR_DISPLAY_NAME), codepage);
        let smtp = text_of(props.get(&PR_SMTP_ADDRESS), codepage);
        let email = text_of(props.get(&PR_EMAIL_ADDRESS), codepage);
        let Some(addr) = addr_candidate([smtp.as_ref(), email.as_ref(), name.as_ref()]) else {
            continue;
        };
        let address = Address {
            name: name.filter(|n| *n != addr),
            addr,
        };
        match int_of(props.get(&PR_RECIPIENT_TYPE)) {
            Some(MAPI_TO) => to.push(address),
            Some(MAPI_CC) => cc.push(address),
            _ => {} // Bcc and originator rows are not shown
        }
    }
    (to, cc)
}

fn bracketed(id: &str) -> String {
    let id = id.trim().trim_matches(['<', '>']);
    format!("<{id}>")
}

/// Outlook stores some subjects with a two-character prefix marker.
fn clean_subject(subject: String) -> String {
    let mut chars = subject.chars();
    if chars.next() == Some('\u{1}') {
        chars.next();
        chars.as_str().to_string()
    } else {
        subject
    }
}

fn html_body(value: Option<&PropertyValue>, codepage: u32) -> Option<String> {
    let html = match value? {
        PropertyValue::Binary(v) => {
            let bytes = v.buffer();
            match std::str::from_utf8(bytes) {
                Ok(s) => s.to_string(),
                Err(_) => tnef::decode_string8(bytes, codepage),
            }
        }
        other => return text_of(Some(other), codepage),
    };
    let html = html.trim_end_matches('\0').to_string();
    Some(html).filter(|h| !h.trim().is_empty())
}

/// Turn a PST message into the header row and body the local store takes.
fn convert(
    message: &dyn Message,
    attachments: &[Rc<dyn Attachment>],
    account_id: &str,
    folder_id: i64,
) -> (NewMessage, ParsedBody) {
    let props = message.properties();
    let codepage = int_of(props.get(PR_INTERNET_CPID))
        .or_else(|| int_of(props.get(PR_MESSAGE_CODEPAGE)))
        .and_then(|cp| u32::try_from(cp).ok())
        .unwrap_or(1252);
    let text = |id: u16| text_of(props.get(id), codepage);

    let flags = int_of(props.get(PR_MESSAGE_FLAGS)).unwrap_or(0);
    let is_read = flags & MSGFLAG_READ != 0;
    let is_starred = int_of(props.get(PR_FLAG_STATUS)) == Some(FOLLOWUP_FLAGGED);
    // When the mail arrived, like IMAP's INTERNALDATE; for sent mail, when it
    // was submitted.
    let date = unix_time(props.get(PR_MESSAGE_DELIVERY_TIME))
        .or_else(|| unix_time(props.get(PR_CLIENT_SUBMIT_TIME)))
        .or_else(|| unix_time(props.get(PR_CREATION_TIME)));
    let size = int_of(props.get(PR_MESSAGE_SIZE)).and_then(|s| u32::try_from(s).ok());

    // The original internet headers are the best source there is: real
    // addresses, threading ids, list and authentication headers.
    let mut msg = match text(PR_TRANSPORT_HEADERS) {
        Some(headers) => parse::parse_headers(
            account_id,
            folder_id,
            0,
            format!("{}\r\n\r\n", headers.trim_end()).as_bytes(),
            date,
            size,
            is_read,
            is_starred,
            false,
        ),
        None => NewMessage {
            account_id: account_id.to_string(),
            folder_id,
            size: size.map(i64::from),
            is_read,
            is_starred,
            date: date.unwrap_or(0),
            ..Default::default()
        },
    };

    if msg.subject.is_none() {
        msg.subject = text(PR_SUBJECT).map(clean_subject);
    }
    if msg.from_addr.is_none() {
        let name = text(PR_SENT_REPRESENTING_NAME).or_else(|| text(PR_SENDER_NAME));
        let smtp = [
            text(PR_SENT_REPRESENTING_SMTP),
            text(PR_SENDER_SMTP),
            text(PR_SENT_REPRESENTING_EMAIL),
            text(PR_SENDER_EMAIL),
        ];
        msg.from_addr = addr_candidate(smtp.iter().map(Option::as_ref));
        if msg.from_name.is_none() {
            msg.from_name = name.filter(|n| Some(n) != msg.from_addr.as_ref());
        }
    }
    if msg.to_addrs.is_empty() && msg.cc_addrs.is_empty() {
        (msg.to_addrs, msg.cc_addrs) = recipients(message, codepage);
    }
    if msg.message_id.is_none() {
        msg.message_id = text(PR_INTERNET_MESSAGE_ID).map(|id| bracketed(&id));
    }
    if msg.in_reply_to.is_none() {
        msg.in_reply_to = text(PR_IN_REPLY_TO_ID).map(|id| bracketed(&id));
    }
    if msg.references.is_empty() {
        msg.references = text(PR_INTERNET_REFERENCES)
            .map(|refs| refs.split_whitespace().map(bracketed).collect())
            .unwrap_or_default();
    }

    let html = html_body(props.get(PR_HTML), codepage);
    let body_text = text(PR_BODY).or_else(|| html.as_deref().map(parse::html_to_text));
    let snippet = body_text
        .as_deref()
        .map(parse::make_snippet)
        .unwrap_or_default();

    let files = attachments
        .iter()
        .filter_map(|a| {
            // Only real files; a message attached to a message is not imported.
            let Some(AttachmentData::Binary(data)) = a.data() else {
                return None;
            };
            let p = a.properties();
            let name = text_of(p.get(PR_ATTACH_LONG_FILENAME), codepage)
                .or_else(|| text_of(p.get(PR_ATTACH_FILENAME), codepage));
            let content_id = text_of(p.get(PR_ATTACH_CONTENT_ID), codepage)
                .map(|c| c.trim_matches(['<', '>', ' ']).to_string())
                .filter(|c| !c.is_empty());
            Some(ParsedAttachment {
                filename: name,
                mime_type: text_of(p.get(PR_ATTACH_MIME_TAG), codepage),
                size: data.buffer().len() as i64,
                // Inline only when the HTML can actually refer to it.
                is_inline: content_id.is_some() && html.is_some(),
                content_id,
                data: data.buffer().to_vec(),
            })
        })
        .collect();

    (
        msg,
        ParsedBody {
            text: body_text,
            html,
            snippet,
            attachments: files,
        },
    )
}

// ---- the import --------------------------------------------------------------

/// Create the tables an import needs. Outside the numbered migrations on
/// purpose, like `pop3_seen`: those numbers belong to upstream.
pub fn ensure_schema(conn: &rusqlite::Connection) -> rusqlite::Result<()> {
    local_store::ensure_schema(conn)?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pst_imported (
           account_id TEXT NOT NULL REFERENCES accounts(id) ON DELETE CASCADE,
           node_id    INTEGER NOT NULL,
           PRIMARY KEY (account_id, node_id)
         ) WITHOUT ROWID;",
    )
}

/// Find or create the sidebar folder for a PST folder path.
fn ensure_folder(
    conn: &rusqlite::Connection,
    account_id: &str,
    path: &[String],
) -> rusqlite::Result<i64> {
    let name = path.join("/");
    // A role is given to the first folder that claims it, and kept.
    let role = role_for(path).filter(|role| {
        local_store::folder_by_role(conn, account_id, role)
            .map(|existing| existing.is_none())
            .unwrap_or(false)
    });
    conn.execute(
        "INSERT OR IGNORE INTO folders (account_id, imap_name, role, display_name, sort_order)
         VALUES (?1, ?2, ?3, ?2, ?4)",
        rusqlite::params![account_id, name, role, local_store::role_sort_order(role)],
    )?;
    conn.query_row(
        "SELECT id FROM folders WHERE account_id = ?1 AND imap_name = ?2",
        rusqlite::params![account_id, name],
        |r| r.get(0),
    )
}

/// Import a PST file into an archive account. Synchronous and long-running:
/// call it from its own thread. `report` is called with running totals;
/// setting `cancel` stops the import after the message in hand.
pub fn run(
    db: &Db,
    data_dir: &Path,
    account_id: &str,
    path: &Path,
    cancel: &AtomicBool,
    report: impl FnMut(&Progress),
) -> Result<Progress> {
    run_with(db, data_dir, account_id, path, cancel, REPORT_EVERY, report)
}

fn run_with(
    db: &Db,
    data_dir: &Path,
    account_id: &str,
    path: &Path,
    cancel: &AtomicBool,
    report_every: u64,
    mut report: impl FnMut(&Progress),
) -> Result<Progress> {
    let source = Source::open(path)?;
    let folders = collect_folders(&source)?;

    let mut progress = Progress {
        account_id: account_id.to_string(),
        phase: Phase::Running,
        total: 0,
        done: 0,
        imported: 0,
        failed: 0,
        message: None,
    };
    progress.total = folders
        .iter()
        .map(|f| u64::try_from(f.folder.properties().content_count().unwrap_or(0)).unwrap_or(0))
        .sum();

    // The special folders first, so the sidebar is complete from the start —
    // then the standard ones for any role the file did not bring.
    let already: HashSet<u32> = db.with(|conn| {
        ensure_schema(conn)?;
        conn.execute(
            "UPDATE accounts SET folder_delimiter = '/' WHERE id = ?1",
            rusqlite::params![account_id],
        )?;
        for f in folders.iter().filter(|f| role_for(&f.path).is_some()) {
            ensure_folder(conn, account_id, &f.path)?;
        }
        local_store::ensure_folders(conn, account_id)?;
        let mut stmt = conn.prepare("SELECT node_id FROM pst_imported WHERE account_id = ?1")?;
        let seen = stmt
            .query_map(rusqlite::params![account_id], |r| r.get::<_, u32>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        Ok(seen)
    })?;
    report(&progress);

    let mut io_failures = 0u32;
    'folders: for pst_folder in &folders {
        let Some(table) = pst_folder.folder.contents_table() else {
            continue;
        };
        // Created on the first message that lands in it, so empty folders
        // (Outbox, RSS Feeds, …) don't clutter the sidebar.
        let mut folder_id: Option<i64> = None;
        for row in table.rows_matrix() {
            if cancel.load(Ordering::Relaxed) {
                progress.phase = Phase::Cancelled;
                break 'folders;
            }
            let node = u32::from(row.id());
            if progress.done > 0 && progress.done.is_multiple_of(report_every) {
                report(&progress);
            }
            progress.done += 1;
            if already.contains(&node) {
                continue;
            }
            let (message, attachments) = match source.message(NodeId::from(node)) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(node, error = %e, "cannot read PST message");
                    progress.failed += 1;
                    continue;
                }
            };
            let class = message.properties().message_class().unwrap_or_default();
            if !is_mail_item(&class) {
                continue;
            }
            let stored = db.with(|conn| {
                let folder = match folder_id {
                    Some(id) => id,
                    None => ensure_folder(conn, account_id, &pst_folder.path)?,
                };
                folder_id = Some(folder);
                let (msg, body) = convert(message.as_ref(), &attachments, account_id, folder);
                if let Err(e) = local_store::store_message(conn, data_dir, msg, body) {
                    return Ok(Err(e));
                }
                // Only once the message is safely in: an interruption costs
                // one duplicate at worst, never a lost message.
                conn.execute(
                    "INSERT OR IGNORE INTO pst_imported (account_id, node_id) VALUES (?1, ?2)",
                    rusqlite::params![account_id, node],
                )?;
                Ok(Ok(()))
            })?;
            match stored {
                Ok(()) => {
                    progress.imported += 1;
                    io_failures = 0;
                }
                Err(e) => {
                    tracing::warn!(node, error = %e, "cannot store PST message");
                    progress.failed += 1;
                    // One message that won't store is that message's problem.
                    // A run of write failures is the disk — full or gone — and
                    // nothing after it would fare better.
                    if matches!(e, SkimError::Io(_)) {
                        io_failures += 1;
                        if io_failures >= MAX_IO_FAILURES {
                            return Err(e);
                        }
                    }
                }
            }
        }
    }

    if progress.phase == Phase::Running {
        progress.phase = Phase::Done;
        progress.done = progress.total.max(progress.done);
    }
    report(&progress);
    Ok(progress)
}

/// What a PST file is, read before an account is created for it.
#[derive(Debug)]
pub struct Identity {
    /// Hex of the store's record key.
    pub key: String,
}

/// Check that `path` is a readable PST and return its identity.
pub fn identify(path: &Path) -> Result<Identity> {
    let source = Source::open(path)?;
    Ok(Identity {
        key: source.record_key()?,
    })
}

// ---- running imports --------------------------------------------------------

struct Running {
    cancel: Arc<AtomicBool>,
    last: Option<Progress>,
}

fn registry() -> &'static Mutex<HashMap<String, Running>> {
    static REGISTRY: OnceLock<Mutex<HashMap<String, Running>>> = OnceLock::new();
    REGISTRY.get_or_init(Default::default)
}

fn setting_key(account_id: &str) -> String {
    format!("pst_import:{account_id}")
}

/// Latest progress of every import that is running.
pub fn status() -> Vec<Progress> {
    registry()
        .lock()
        .map(|r| r.values().filter_map(|run| run.last.clone()).collect())
        .unwrap_or_default()
}

/// Ask a running import to stop. It will not resume by itself afterwards;
/// importing the same file again continues it.
pub fn cancel(account_id: &str) {
    if let Ok(registry) = registry().lock() {
        if let Some(run) = registry.get(account_id) {
            run.cancel.store(true, Ordering::Relaxed);
        }
    }
}

/// Start importing `path` into `account_id` on a background thread. A second
/// call for an account that is already importing does nothing.
pub fn launch(app: &AppHandle, db: &Db, data_dir: &Path, account_id: &str, path: PathBuf) {
    let cancel = Arc::new(AtomicBool::new(false));
    {
        let Ok(mut registry) = registry().lock() else {
            return;
        };
        if registry.contains_key(account_id) {
            return;
        }
        registry.insert(
            account_id.to_string(),
            Running {
                cancel: cancel.clone(),
                last: None,
            },
        );
    }
    let (app, db, data_dir, account_id) = (
        app.clone(),
        db.clone(),
        data_dir.to_path_buf(),
        account_id.to_string(),
    );
    std::thread::spawn(move || {
        // Remembered until the import finishes, so a restart can resume it.
        let key = setting_key(&account_id);
        let source = path.to_string_lossy().into_owned();
        let _ = db.with(|conn| crate::db::queries::set_setting(conn, &key, &source));

        let mut folders_seen = 0u64;
        let outcome = run(&db, &data_dir, &account_id, &path, &cancel, |progress| {
            if let Ok(mut registry) = registry().lock() {
                if let Some(run) = registry.get_mut(&progress.account_id) {
                    run.last = Some(progress.clone());
                }
            }
            let _ = app.emit("import:progress", progress);
            // The list fills in as the import goes; the sidebar gains folders.
            if progress.imported > folders_seen {
                folders_seen = progress.imported;
                let _ = app.emit("folders:updated", serde_json::json!({}));
                let _ = app.emit("mail:updated", serde_json::json!({}));
            }
        });
        let last = match outcome {
            Ok(progress) => progress,
            Err(e) => {
                tracing::warn!(error = %e, "PST import failed");
                Progress {
                    account_id: account_id.clone(),
                    phase: Phase::Error,
                    total: 0,
                    done: 0,
                    imported: 0,
                    failed: 0,
                    message: Some(e.to_string()),
                }
            }
        };
        // Finished or deliberately stopped: nothing to resume. After an error
        // (the file was on a drive that went away) the next start tries again.
        if last.phase != Phase::Error {
            let _ = db.with(|conn| {
                conn.execute(
                    "DELETE FROM settings WHERE key = ?1",
                    rusqlite::params![key],
                )
            });
        }
        if let Ok(mut registry) = registry().lock() {
            registry.remove(&account_id);
        }
        let _ = app.emit("import:progress", &last);
        let _ = app.emit("folders:updated", serde_json::json!({}));
        let _ = app.emit("mail:updated", serde_json::json!({}));
        tauri::async_runtime::spawn(async move { crate::badge::refresh(&app).await });
    });
}

/// Continue an import that did not finish, if this account has one.
pub fn resume(app: &AppHandle, db: &Db, data_dir: &Path, account_id: &str) {
    let key = setting_key(account_id);
    let path = db
        .with(|conn| crate::db::queries::get_setting(conn, &key))
        .ok()
        .flatten();
    if let Some(path) = path.filter(|p| Path::new(p).is_file()) {
        launch(app, db, data_dir, account_id, PathBuf::from(path));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn filetime_converts_to_unix_seconds() {
        // 2026-03-03T02:00:00Z
        let ticks = PropertyValue::Time(134_169_768_000_000_000);
        assert_eq!(unix_time(Some(&ticks)), Some(1_772_503_200));
        assert_eq!(unix_time(Some(&PropertyValue::Time(0))), None);
        assert_eq!(unix_time(None), None);
    }

    #[test]
    fn mail_folders_are_told_from_the_rest() {
        assert!(is_mail_folder(None));
        assert!(is_mail_folder(Some("IPF.Note")));
        assert!(is_mail_folder(Some("IPF.Note.OutlookHomepage")));
        assert!(!is_mail_folder(Some("IPF.Appointment")));
        assert!(!is_mail_folder(Some("IPF.Contact")));
        assert!(!is_mail_folder(Some("IPF.Task")));
    }

    #[test]
    fn mail_items_are_told_from_the_rest() {
        assert!(is_mail_item("IPM.Note"));
        assert!(is_mail_item("IPM.Note.SMIME.MultipartSigned"));
        assert!(is_mail_item("IPM.Schedule.Meeting.Request"));
        assert!(is_mail_item("REPORT.IPM.Note.NDR"));
        assert!(!is_mail_item("IPM.Contact"));
        assert!(!is_mail_item("IPM.Appointment"));
        assert!(!is_mail_item("IPM.Task"));
        assert!(!is_mail_item("IPM.StickyNote"));
    }

    #[test]
    fn roles_come_from_top_level_names_in_three_languages() {
        let path = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(role_for(&path(&["Inbox"])), Some("inbox"));
        assert_eq!(role_for(&path(&["收件匣"])), Some("inbox"));
        assert_eq!(role_for(&path(&["Kotak Masuk"])), Some("inbox"));
        assert_eq!(role_for(&path(&["Sent Items"])), Some("sent"));
        assert_eq!(role_for(&path(&["寄件備份"])), Some("sent"));
        assert_eq!(role_for(&path(&["Deleted Items"])), Some("trash"));
        assert_eq!(role_for(&path(&["Projects"])), None);
        assert_eq!(
            role_for(&path(&["Projects", "Inbox"])),
            None,
            "only at the top"
        );
    }

    #[test]
    fn an_smtp_address_is_preferred_over_a_directory_name() {
        let dn = "/O=ORG/OU=SITE/CN=RECIPIENTS/CN=ANN".to_string();
        let smtp = "ann@example.com".to_string();
        assert_eq!(
            addr_candidate([Some(&dn), Some(&smtp)]).as_deref(),
            Some("ann@example.com")
        );
        assert_eq!(
            addr_candidate([Some(&dn), None]).as_deref(),
            Some(dn.as_str())
        );
        assert_eq!(addr_candidate([None, None]), None);
    }

    #[test]
    fn subject_prefix_markers_are_removed() {
        assert_eq!(clean_subject("\u{1}\u{5}RE: Plan".into()), "RE: Plan");
        assert_eq!(clean_subject("Plan".into()), "Plan");
    }

    #[test]
    fn message_ids_are_bracketed_once() {
        assert_eq!(bracketed("a@b"), "<a@b>");
        assert_eq!(bracketed(" <a@b> "), "<a@b>");
    }

    /// A small Unicode PST built from four `.eml` files: two in Inbox (a mail
    /// with a Chinese subject, HTML and two attachments, and a reply to it),
    /// two in Projects (plain text; HTML with an inline image).
    fn fixture() -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.pst")
    }

    fn archive_db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.with(|conn| {
            conn.execute(
                "INSERT INTO accounts (id, email, provider, imap_host, smtp_host, created_at)
                 VALUES ('arc', 'sample.pst', 'local', 'sample.pst', '', 0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        db
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("skim-pst-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn import(db: &Db, dir: &Path) -> Progress {
        run(db, dir, "arc", &fixture(), &AtomicBool::new(false), |_| {}).unwrap()
    }

    fn run_reporting_every(
        db: &Db,
        dir: &Path,
        cancel: &AtomicBool,
        every: u64,
        report: impl FnMut(&Progress),
    ) -> Progress {
        run_with(db, dir, "arc", &fixture(), cancel, every, report).unwrap()
    }

    fn count(db: &Db, sql: &str) -> i64 {
        let sql = sql.to_string();
        db.with(move |conn| conn.query_row(&sql, [], |r| r.get(0)))
            .unwrap()
    }

    #[test]
    fn the_fixture_has_a_stable_identity() {
        let a = identify(&fixture()).unwrap();
        let b = identify(&fixture()).unwrap();
        assert_eq!(a.key.len(), 32);
        assert_eq!(a.key, b.key);
    }

    #[test]
    fn imports_every_message_into_its_folder() {
        let db = archive_db();
        let dir = temp_dir("all");
        let p = import(&db, &dir);
        assert_eq!(p.phase, Phase::Done);
        assert_eq!((p.total, p.done, p.imported, p.failed), (4, 4, 4, 0));

        let inbox = count(
            &db,
            "SELECT count(*) FROM messages m JOIN folders f ON f.id = m.folder_id
             WHERE f.role = 'inbox'",
        );
        let projects = count(
            &db,
            "SELECT count(*) FROM messages m JOIN folders f ON f.id = m.folder_id
             WHERE f.imap_name = 'Projects'",
        );
        assert_eq!((inbox, projects), (2, 2));
    }

    #[test]
    fn the_sidebar_gets_one_folder_per_role_and_no_empty_extras() {
        let db = archive_db();
        import(&db, &temp_dir("folders"));
        for role in ["inbox", "sent", "archive", "junk", "trash"] {
            let n = count(
                &db,
                &format!("SELECT count(*) FROM folders WHERE role = '{role}'"),
            );
            assert_eq!(n, 1, "exactly one {role} folder");
        }
        // The file's own "Deleted Items" is the trash; no second one is made.
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM folders WHERE imap_name = 'Deleted Items' AND role = 'trash'"
            ),
            1
        );
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM folders WHERE imap_name = 'Trash'"
            ),
            0
        );
        let delimiter: Option<String> = db
            .with(|conn| conn.query_row("SELECT folder_delimiter FROM accounts", [], |r| r.get(0)))
            .unwrap();
        assert_eq!(delimiter.as_deref(), Some("/"));
    }

    #[test]
    fn headers_body_and_attachments_survive() {
        let db = archive_db();
        let dir = temp_dir("content");
        import(&db, &dir);

        type Row = (i64, String, String, String, String, String, i64, i64, i64);
        let (pk, from_name, from_addr, to, cc, snippet, date, state, has_att): Row = db
            .with(|conn| {
                conn.query_row(
                    "SELECT id, from_name, from_addr, to_addrs, cc_addrs, snippet, date,
                            body_state, has_attachments
                     FROM messages WHERE subject = '季度報告 Q3'",
                    [],
                    |r| {
                        Ok((
                            r.get(0)?,
                            r.get(1)?,
                            r.get(2)?,
                            r.get(3)?,
                            r.get(4)?,
                            r.get(5)?,
                            r.get(6)?,
                            r.get(7)?,
                            r.get(8)?,
                        ))
                    },
                )
            })
            .unwrap();
        assert_eq!(from_name, "王小明");
        assert_eq!(from_addr, "ming@example.com");
        assert!(
            to.contains("chen@example.com") && to.contains("team@example.com"),
            "{to}"
        );
        assert!(cc.contains("boss@example.com"), "{cc}");
        assert!(snippet.contains("第三季"), "{snippet}");
        assert_eq!(date, 1_772_503_200, "2026-03-03 10:00 +08:00");
        assert_eq!(state, 1);
        assert_eq!(has_att, 1);

        let (html, text) = db
            .with(|conn| crate::db::bodies::get_body(conn, pk))
            .unwrap()
            .unwrap();
        assert!(html.unwrap().contains("<b>第三季</b>"));
        assert!(text.unwrap().contains("請查收"));

        let files: Vec<(String, String, i64, String)> = db
            .with(|conn| {
                let mut stmt = conn.prepare(
                    "SELECT filename, mime_type, size, cache_path FROM attachments
                     WHERE message_id = ?1 ORDER BY filename",
                )?;
                let rows = stmt
                    .query_map(rusqlite::params![pk], |r| {
                        Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                Ok(rows)
            })
            .unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.0.as_str()).collect();
        assert_eq!(names, ["summary.pdf", "報告.csv"]);
        assert_eq!(files[0].1, "application/pdf");
        assert_eq!(
            std::fs::read(&files[0].3).unwrap(),
            b"%PDF-1.4 fake pdf bytes",
            "the file on disk is the attachment"
        );
        assert_eq!(files[1].2, 16);
    }

    #[test]
    fn a_reply_joins_the_thread_of_the_message_it_answers() {
        let db = archive_db();
        import(&db, &temp_dir("thread"));
        let threads = count(
            &db,
            "SELECT count(DISTINCT thread_id) FROM messages WHERE subject LIKE '%季度報告%'",
        );
        assert_eq!(threads, 1);
        assert_eq!(count(&db, "SELECT count(*) FROM threads"), 3);
    }

    #[test]
    fn an_inline_image_is_inline_and_read_state_is_kept() {
        let db = archive_db();
        import(&db, &temp_dir("inline"));
        let inline = count(
            &db,
            "SELECT is_inline FROM attachments WHERE filename = 'logo.png' AND content_id = 'logo1'",
        );
        assert_eq!(inline, 1);
        // The converter marks everything read; nothing should show as new.
        assert_eq!(
            count(&db, "SELECT count(*) FROM messages WHERE is_read = 0"),
            0
        );
    }

    #[test]
    fn imported_mail_is_searchable() {
        let db = archive_db();
        import(&db, &temp_dir("search"));
        assert_eq!(
            count(
                &db,
                "SELECT count(*) FROM messages_fts WHERE messages_fts MATCH 'rapat'"
            ),
            1
        );
    }

    #[test]
    fn importing_the_same_file_again_adds_nothing() {
        let db = archive_db();
        let dir = temp_dir("again");
        import(&db, &dir);
        let second = import(&db, &dir);
        assert_eq!(second.phase, Phase::Done);
        assert_eq!((second.done, second.imported), (4, 0));
        assert_eq!(count(&db, "SELECT count(*) FROM messages"), 4);
    }

    #[test]
    fn a_cancelled_import_resumes_where_it_stopped() {
        let db = archive_db();
        let dir = temp_dir("resume");
        // Cancelled after the second message is in.
        let cancel = AtomicBool::new(false);
        let mut stored = 0;
        let stopped = run_reporting_every(&db, &dir, &cancel, 1, |p| {
            stored = p.imported;
            if p.imported >= 2 {
                cancel.store(true, Ordering::Relaxed);
            }
        });
        assert_eq!(stopped.phase, Phase::Cancelled);
        assert!((2..4).contains(&stopped.imported), "{stopped:?}");
        let before = count(&db, "SELECT count(*) FROM messages");
        assert_eq!(before as u64, stopped.imported);

        let resumed = import(&db, &dir);
        assert_eq!(resumed.phase, Phase::Done);
        assert_eq!(resumed.imported, 4 - stopped.imported, "only the rest");
        assert_eq!(count(&db, "SELECT count(*) FROM messages"), 4);
        let _ = stored;
    }

    #[test]
    fn a_missing_file_is_an_error_not_a_panic() {
        let db = archive_db();
        let e = run(
            &db,
            &temp_dir("missing"),
            "arc",
            Path::new("no-such-file.pst"),
            &AtomicBool::new(false),
            |_| {},
        )
        .expect_err("must fail");
        assert_eq!(e.code(), "pst");
    }

    /// Manual check against a real file, on a real on-disk database:
    /// `SKIM_TEST_PST=path\\to\\file.pst cargo test --lib -- --ignored real_pst --nocapture`
    #[test]
    #[ignore = "needs SKIM_TEST_PST pointing at a PST file"]
    fn real_pst() {
        let Ok(path) = std::env::var("SKIM_TEST_PST") else {
            return;
        };
        let dir = temp_dir("real");
        let db = Db::open(&dir.join("skim.db")).unwrap();
        db.with(|conn| {
            conn.execute(
                "INSERT INTO accounts (id, email, provider, imap_host, smtp_host, created_at)
                 VALUES ('arc', 'real.pst', 'local', 'real.pst', '', 0)",
                [],
            )?;
            Ok(())
        })
        .unwrap();
        let started = std::time::Instant::now();
        // Rate per slice, so a slowdown as the database grows is visible.
        let mut lap = (std::time::Instant::now(), 0u64);
        let p = run_with(
            &db,
            &dir,
            "arc",
            Path::new(&path),
            &AtomicBool::new(false),
            5000,
            |p| {
                let secs = lap.0.elapsed().as_secs_f64().max(0.001);
                if p.imported > lap.1 {
                    println!(
                        "  at {}: {:.0} msg/s",
                        p.imported,
                        (p.imported - lap.1) as f64 / secs
                    );
                }
                lap = (std::time::Instant::now(), p.imported);
            },
        )
        .unwrap();
        let secs = started.elapsed().as_secs_f64();
        println!(
            "{:?}: {} of {} imported, {} failed, {:.1}s ({:.0} msg/s)",
            p.phase,
            p.imported,
            p.total,
            p.failed,
            secs,
            p.imported as f64 / secs.max(0.001)
        );
        for (label, sql) in [
            ("folders", "SELECT count(*) FROM folders"),
            ("threads", "SELECT count(*) FROM threads"),
            ("attachments", "SELECT count(*) FROM attachments"),
            (
                "no body",
                "SELECT count(*) FROM messages WHERE body_state <> 1",
            ),
            (
                "no sender",
                "SELECT count(*) FROM messages WHERE from_addr IS NULL",
            ),
        ] {
            println!("  {label}: {}", count(&db, sql));
        }
    }

    #[test]
    fn a_file_that_is_not_a_pst_is_refused_with_a_clear_error() {
        let path = std::env::temp_dir().join(format!("skim-not-a-pst-{}", std::process::id()));
        std::fs::write(&path, b"this is just text").unwrap();
        let e = identify(&path).expect_err("must be refused");
        assert_eq!(e.code(), "pst");
        let _ = std::fs::remove_file(&path);
    }
}
