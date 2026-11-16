//! Address book.
//!
//! Fork addition. Upstream Skim deliberately has no contacts; this build keeps
//! a small local one because the people using it come from Outlook and
//! Thunderbird and bring an address book with them.
//!
//! It stays small on purpose: a contact is a name and one address, with a
//! company, a phone number and a note for context. A group is a named set of
//! contacts that the recipient field expands into its members. Everything is
//! local — nothing here talks to a server.
//!
//! The tables are created on demand, outside upstream's numbered migrations.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub fn ensure_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pcg_contacts (
           id         INTEGER PRIMARY KEY,
           name       TEXT NOT NULL DEFAULT '',
           email      TEXT NOT NULL COLLATE NOCASE UNIQUE,
           company    TEXT,
           phone      TEXT,
           note       TEXT,
           created_at INTEGER NOT NULL DEFAULT (unixepoch())
         );
         CREATE TABLE IF NOT EXISTS pcg_contact_groups (
           id   INTEGER PRIMARY KEY,
           name TEXT NOT NULL COLLATE NOCASE UNIQUE
         );
         CREATE TABLE IF NOT EXISTS pcg_contact_group_members (
           group_id   INTEGER NOT NULL REFERENCES pcg_contact_groups(id) ON DELETE CASCADE,
           contact_id INTEGER NOT NULL REFERENCES pcg_contacts(id) ON DELETE CASCADE,
           PRIMARY KEY (group_id, contact_id)
         ) WITHOUT ROWID;",
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Contact {
    /// 0 for a contact that is not stored yet.
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub company: Option<String>,
    #[serde(default)]
    pub phone: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: i64,
    pub name: String,
    pub member_ids: Vec<i64>,
}

fn clean(value: Option<String>) -> Option<String> {
    value
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A usable address: one `@` with something on both sides and no spaces.
pub fn valid_email(email: &str) -> bool {
    let email = email.trim();
    match email.split_once('@') {
        Some((local, domain)) => {
            !local.is_empty()
                && domain.contains('.')
                && !domain.starts_with('.')
                && !domain.ends_with('.')
                && !email
                    .chars()
                    .any(|c| c.is_whitespace() || "<>,;\"".contains(c))
                && !domain.contains('@')
        }
        None => false,
    }
}

fn row_to_contact(r: &rusqlite::Row<'_>) -> rusqlite::Result<Contact> {
    Ok(Contact {
        id: r.get(0)?,
        name: r.get(1)?,
        email: r.get(2)?,
        company: r.get(3)?,
        phone: r.get(4)?,
        note: r.get(5)?,
    })
}

const COLS: &str = "id, name, email, company, phone, note";

/// Every contact, by name; ones without a name sort by address.
pub fn list(conn: &Connection) -> rusqlite::Result<Vec<Contact>> {
    let mut stmt = conn.prepare_cached(&format!(
        "SELECT {COLS} FROM pcg_contacts
         ORDER BY CASE WHEN name = '' THEN email ELSE name END COLLATE NOCASE"
    ))?;
    let rows = stmt
        .query_map([], row_to_contact)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// What became of a contact that was saved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Saved {
    Added,
    Updated,
}

/// Store a contact. With an id it is edited in place; without one it is added,
/// or merged into the contact that already has this address — an address book
/// with the same person in it twice helps nobody. On a merge, fields the
/// incoming contact leaves empty keep their stored value.
pub fn save(conn: &Connection, contact: &Contact) -> rusqlite::Result<(Contact, Saved)> {
    let email = contact.email.trim().to_string();
    let name = contact.name.trim().to_string();
    let company = clean(contact.company.clone());
    let phone = clean(contact.phone.clone());
    let note = clean(contact.note.clone());

    if contact.id != 0 {
        conn.execute(
            "UPDATE pcg_contacts SET name = ?2, email = ?3, company = ?4, phone = ?5, note = ?6
             WHERE id = ?1",
            params![contact.id, name, email, company, phone, note],
        )?;
        return Ok((get(conn, contact.id)?, Saved::Updated));
    }
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM pcg_contacts WHERE email = ?1",
            params![email],
            |r| r.get(0),
        )
        .optional()?;
    match existing {
        Some(id) => {
            conn.execute(
                "UPDATE pcg_contacts SET
                   name    = CASE WHEN ?2 = '' THEN name ELSE ?2 END,
                   company = COALESCE(?3, company),
                   phone   = COALESCE(?4, phone),
                   note    = COALESCE(?5, note)
                 WHERE id = ?1",
                params![id, name, company, phone, note],
            )?;
            Ok((get(conn, id)?, Saved::Updated))
        }
        None => {
            conn.execute(
                "INSERT INTO pcg_contacts (name, email, company, phone, note)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![name, email, company, phone, note],
            )?;
            Ok((get(conn, conn.last_insert_rowid())?, Saved::Added))
        }
    }
}

fn get(conn: &Connection, id: i64) -> rusqlite::Result<Contact> {
    conn.query_row(
        &format!("SELECT {COLS} FROM pcg_contacts WHERE id = ?1"),
        params![id],
        row_to_contact,
    )
}

pub fn delete(conn: &Connection, ids: &[i64]) -> rusqlite::Result<()> {
    for id in ids {
        conn.execute("DELETE FROM pcg_contacts WHERE id = ?1", params![id])?;
    }
    Ok(())
}

pub fn list_groups(conn: &Connection) -> rusqlite::Result<Vec<Group>> {
    let mut groups: Vec<Group> = conn
        .prepare_cached("SELECT id, name FROM pcg_contact_groups ORDER BY name COLLATE NOCASE")?
        .query_map([], |r| {
            Ok(Group {
                id: r.get(0)?,
                name: r.get(1)?,
                member_ids: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut members = conn
        .prepare_cached("SELECT contact_id FROM pcg_contact_group_members WHERE group_id = ?1")?;
    for group in &mut groups {
        group.member_ids = members
            .query_map(params![group.id], |r| r.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
    }
    Ok(groups)
}

/// Create a group (`id` 0) or rename it, and set its members.
pub fn save_group(
    conn: &mut Connection,
    id: i64,
    name: &str,
    member_ids: &[i64],
) -> rusqlite::Result<Group> {
    let name = name.trim();
    let tx = conn.transaction()?;
    let id = if id == 0 {
        tx.execute(
            "INSERT INTO pcg_contact_groups (name) VALUES (?1)",
            params![name],
        )?;
        tx.last_insert_rowid()
    } else {
        tx.execute(
            "UPDATE pcg_contact_groups SET name = ?2 WHERE id = ?1",
            params![id, name],
        )?;
        id
    };
    tx.execute(
        "DELETE FROM pcg_contact_group_members WHERE group_id = ?1",
        params![id],
    )?;
    for contact_id in member_ids {
        tx.execute(
            "INSERT OR IGNORE INTO pcg_contact_group_members (group_id, contact_id)
             SELECT ?1, id FROM pcg_contacts WHERE id = ?2",
            params![id, contact_id],
        )?;
    }
    tx.commit()?;
    Ok(list_groups(conn)?
        .into_iter()
        .find(|g| g.id == id)
        .unwrap_or(Group {
            id,
            name: name.to_string(),
            member_ids: Vec::new(),
        }))
}

pub fn delete_group(conn: &Connection, id: i64) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM pcg_contact_groups WHERE id = ?1", params![id])?;
    Ok(())
}

/// A recipient suggestion from the address book.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Suggestion {
    pub name: Option<String>,
    /// One address, or for a group every member's, comma-separated — which is
    /// exactly what the recipient field takes.
    pub addr: String,
}

/// Groups and contacts matching what is being typed. `like` is an escaped
/// `%query%` pattern (escape character `\`). Groups come first: typing a
/// group's name is asking for the group.
pub fn suggest(conn: &Connection, like: &str, limit: usize) -> rusqlite::Result<Vec<Suggestion>> {
    let mut out = Vec::new();
    let mut groups = conn.prepare_cached(
        "SELECT g.name, group_concat(c.email, ', ')
         FROM pcg_contact_groups g
         JOIN pcg_contact_group_members m ON m.group_id = g.id
         JOIN pcg_contacts c ON c.id = m.contact_id
         WHERE g.name LIKE ?1 ESCAPE '\\'
         GROUP BY g.id ORDER BY g.name COLLATE NOCASE LIMIT 3",
    )?;
    for row in groups.query_map(params![like], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (name, addr) = row?;
        out.push(Suggestion {
            name: Some(name),
            addr,
        });
    }
    let mut contacts = conn.prepare_cached(
        "SELECT name, email FROM pcg_contacts
         WHERE email LIKE ?1 ESCAPE '\\' OR name LIKE ?1 ESCAPE '\\'
            OR COALESCE(company, '') LIKE ?1 ESCAPE '\\'
         ORDER BY CASE WHEN name = '' THEN email ELSE name END COLLATE NOCASE
         LIMIT ?2",
    )?;
    for row in contacts.query_map(params![like, limit as i64], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })? {
        let (name, addr) = row?;
        out.push(Suggestion {
            name: Some(name).filter(|n| !n.is_empty()),
            addr,
        });
    }
    out.truncate(limit);
    Ok(out)
}

// ---- import -----------------------------------------------------------------

/// A contact read from a file, with the groups it says it belongs to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Imported {
    pub contact: Contact,
    pub groups: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ImportReport {
    pub added: usize,
    pub updated: usize,
    /// Entries with no usable e-mail address.
    pub skipped: usize,
}

/// Store what a file held: contacts merged by address, groups created as
/// named. One transaction, so a failure half-way leaves the book untouched.
pub fn import(
    conn: &mut Connection,
    entries: &[Imported],
    skipped: usize,
) -> rusqlite::Result<ImportReport> {
    let tx = conn.transaction()?;
    let mut report = ImportReport {
        skipped,
        ..Default::default()
    };
    let mut memberships: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for entry in entries {
        let (stored, outcome) = save(&tx, &entry.contact)?;
        match outcome {
            Saved::Added => report.added += 1,
            Saved::Updated => report.updated += 1,
        }
        for group in &entry.groups {
            memberships
                .entry(group.trim().to_string())
                .or_default()
                .push(stored.id);
        }
    }
    for (name, members) in memberships {
        if name.is_empty() {
            continue;
        }
        tx.execute(
            "INSERT OR IGNORE INTO pcg_contact_groups (name) VALUES (?1)",
            params![name],
        )?;
        let group_id: i64 = tx.query_row(
            "SELECT id FROM pcg_contact_groups WHERE name = ?1",
            params![name],
            |r| r.get(0),
        )?;
        for contact_id in members {
            tx.execute(
                "INSERT OR IGNORE INTO pcg_contact_group_members (group_id, contact_id)
                 VALUES (?1, ?2)",
                params![group_id, contact_id],
            )?;
        }
    }
    tx.commit()?;
    Ok(report)
}

/// Decode an address-book file to text. Outlook writes UTF-16 with a byte
/// order mark for its "Unicode" exports and the machine's legacy code page for
/// the rest, with nothing in the file to say which; `fallback` is the best
/// guess for that case (see [`legacy_encoding`]).
pub fn decode_file(bytes: &[u8], fallback: &'static encoding_rs::Encoding) -> String {
    if let Some((encoding, bom_len)) = encoding_rs::Encoding::for_bom(bytes) {
        return encoding.decode(&bytes[bom_len..]).0.into_owned();
    }
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => fallback.decode(bytes).0.into_owned(),
    }
}

/// The legacy code page a Windows machine in this UI language most likely
/// used to write a file that is not Unicode.
pub fn legacy_encoding(locale: &str) -> &'static encoding_rs::Encoding {
    use encoding_rs::*;
    match locale {
        "zh-TW" => BIG5,
        "zh" => GBK,
        "ja" => SHIFT_JIS,
        "ko" => EUC_KR,
        "ru" | "sr" => WINDOWS_1251,
        "pl" => WINDOWS_1250,
        _ => WINDOWS_1252,
    }
}

/// Read contacts from file text, as vCard or CSV — decided by content, since
/// extensions lie. Returns the contacts and how many entries had no address.
pub fn parse_file(text: &str) -> (Vec<Imported>, usize) {
    if text.to_ascii_uppercase().contains("BEGIN:VCARD") {
        parse_vcards(text)
    } else {
        parse_csv(text)
    }
}

fn is_cjk(text: &str) -> bool {
    text.chars().any(|c| {
        matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF)
    })
}

/// Put a given and a family name in the order their language writes them.
pub(crate) fn full_name(given: &str, family: &str) -> String {
    let (given, family) = (given.trim(), family.trim());
    if given.is_empty() || family.is_empty() {
        return format!("{given}{family}");
    }
    if is_cjk(given) || is_cjk(family) {
        format!("{family}{given}")
    } else {
        format!("{given} {family}")
    }
}

// ---- vCard ---------------------------------------------------------------------

fn decode_quoted_printable(value: &str) -> Vec<u8> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let escaped = (bytes[i] == b'=')
            .then(|| bytes.get(i + 1..i + 3))
            .flatten()
            .and_then(|hex| std::str::from_utf8(hex).ok())
            .and_then(|hex| u8::from_str_radix(hex, 16).ok());
        match escaped {
            Some(byte) => {
                out.push(byte);
                i += 3;
            }
            None => {
                out.push(bytes[i]);
                i += 1;
            }
        }
    }
    out
}

/// Split a vCard value on `separator`, leaving escaped separators (`\;`,
/// `\,`) inside their part, and unescape each part.
fn split_vcard(value: &str, separator: char) -> Vec<String> {
    let mut parts = vec![String::new()];
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        let last = parts.last_mut().expect("never empty");
        if c == '\\' {
            last.push(c);
            last.extend(chars.next());
        } else if c == separator {
            parts.push(String::new());
        } else {
            last.push(c);
        }
    }
    parts.iter().map(|p| unescape_vcard(p)).collect()
}

fn unescape_vcard(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') | Some('N') => out.push('\n'),
            Some(other) => out.push(other),
            None => {}
        }
    }
    out
}

/// Unfold a vCard into logical lines: a line starting with a space or tab
/// continues the one before, and a quoted-printable line ending in `=` does too.
fn vcard_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let mut soft_break = false;
    for raw in text.lines() {
        let folded = raw.starts_with(' ') || raw.starts_with('\t');
        if let (true, Some(last)) = (folded || soft_break, lines.last_mut()) {
            last.push_str(if soft_break { raw } else { &raw[1..] });
        } else {
            lines.push(raw.to_string());
        }
        soft_break = false;
        if let Some(last) = lines.last_mut() {
            let qp = last
                .split_once(':')
                .is_some_and(|(head, _)| head.to_ascii_uppercase().contains("QUOTED-PRINTABLE"));
            if qp && last.ends_with('=') {
                last.pop();
                soft_break = true;
            }
        }
    }
    lines
}

fn parse_vcards(text: &str) -> (Vec<Imported>, usize) {
    #[derive(Default)]
    struct Card {
        formatted: String,
        structured: String,
        emails: Vec<String>,
        company: Option<String>,
        phone: Option<String>,
        note: Option<String>,
        groups: Vec<String>,
    }
    let mut out = Vec::new();
    let mut skipped = 0;
    let mut card: Option<Card> = None;

    for line in vcard_lines(text) {
        let Some((head, raw_value)) = line.split_once(':') else {
            continue;
        };
        let mut params = head.split(';');
        // "item1.EMAIL" (Apple, Google) is still EMAIL.
        let name = params
            .next()
            .unwrap_or_default()
            .trim()
            .to_ascii_uppercase();
        let name = name.rsplit('.').next().unwrap_or_default().to_string();
        let params: Vec<String> = params.map(|p| p.trim().to_ascii_uppercase()).collect();

        if name == "BEGIN" && raw_value.trim().eq_ignore_ascii_case("VCARD") {
            card = Some(Card::default());
            continue;
        }
        if name == "END" && raw_value.trim().eq_ignore_ascii_case("VCARD") {
            let Some(done) = card.take() else { continue };
            let name = if done.formatted.trim().is_empty() {
                done.structured
            } else {
                done.formatted
            };
            let emails: Vec<String> = done.emails.into_iter().filter(|e| valid_email(e)).collect();
            if emails.is_empty() {
                skipped += 1;
            }
            // An export of a nameless contact names it after its address.
            let name = if emails.iter().any(|e| e.eq_ignore_ascii_case(name.trim())) {
                String::new()
            } else {
                name
            };
            // One contact per address: the book is keyed on the address.
            for email in emails {
                out.push(Imported {
                    contact: Contact {
                        id: 0,
                        name: name.trim().to_string(),
                        email,
                        company: done.company.clone(),
                        phone: done.phone.clone(),
                        note: done.note.clone(),
                    },
                    groups: done.groups.clone(),
                });
            }
            continue;
        }
        let Some(current) = card.as_mut() else {
            continue;
        };

        let value = if params.iter().any(|p| p.ends_with("QUOTED-PRINTABLE")) {
            let bytes = decode_quoted_printable(raw_value);
            let charset = params
                .iter()
                .find_map(|p| p.strip_prefix("CHARSET="))
                .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()));
            match (std::str::from_utf8(&bytes), charset) {
                (Ok(text), _) => text.to_string(),
                (Err(_), Some(encoding)) => encoding.decode(&bytes).0.into_owned(),
                (Err(_), None) => String::from_utf8_lossy(&bytes).into_owned(),
            }
        } else {
            raw_value.to_string()
        };

        match name.as_str() {
            "FN" => current.formatted = unescape_vcard(&value),
            "N" => {
                let mut parts = split_vcard(&value, ';').into_iter();
                let family = parts.next().unwrap_or_default();
                let given = parts.next().unwrap_or_default();
                current.structured = full_name(&given, &family);
            }
            "EMAIL" => current
                .emails
                .push(unescape_vcard(&value).trim().to_string()),
            "ORG" => {
                let org = split_vcard(&value, ';').into_iter().next();
                current.company = current.company.take().or(clean(org));
            }
            "TEL" => {
                current.phone = current.phone.take().or(clean(Some(unescape_vcard(&value))));
            }
            "NOTE" => current.note = clean(Some(unescape_vcard(&value))),
            "CATEGORIES" => {
                let names = split_vcard(&value, ',');
                current.groups.extend(
                    names
                        .iter()
                        .map(|g| g.trim().to_string())
                        .filter(|g| !g.is_empty()),
                );
            }
            _ => {}
        }
    }
    (out, skipped)
}

fn escape_vcard(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "")
        .replace(',', "\\,")
        .replace(';', "\\;")
}

/// Write the whole book as vCard 3.0 in UTF-8 — the one format Outlook,
/// Thunderbird, phones and webmail all read. Groups travel as `CATEGORIES`.
pub fn export_vcards(contacts: &[Contact], groups: &[Group]) -> String {
    let mut out = String::new();
    for c in contacts {
        let display = if c.name.is_empty() { &c.email } else { &c.name };
        out.push_str("BEGIN:VCARD\r\nVERSION:3.0\r\n");
        out.push_str(&format!("FN:{}\r\n", escape_vcard(display)));
        out.push_str(&format!("N:{};;;;\r\n", escape_vcard(display)));
        out.push_str(&format!(
            "EMAIL;TYPE=INTERNET:{}\r\n",
            escape_vcard(&c.email)
        ));
        if let Some(company) = &c.company {
            out.push_str(&format!("ORG:{}\r\n", escape_vcard(company)));
        }
        if let Some(phone) = &c.phone {
            out.push_str(&format!("TEL:{}\r\n", escape_vcard(phone)));
        }
        if let Some(note) = &c.note {
            out.push_str(&format!("NOTE:{}\r\n", escape_vcard(note)));
        }
        let names: Vec<String> = groups
            .iter()
            .filter(|g| g.member_ids.contains(&c.id))
            .map(|g| escape_vcard(&g.name))
            .collect();
        if !names.is_empty() {
            out.push_str(&format!("CATEGORIES:{}\r\n", names.join(",")));
        }
        out.push_str("END:VCARD\r\n");
    }
    out
}

// ---- CSV -----------------------------------------------------------------------

/// Split CSV text into rows of fields. Quoted fields may hold the delimiter,
/// doubled quotes and line breaks.
fn csv_rows(text: &str, delimiter: char) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row: Vec<String> = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    field.push('"');
                    chars.next();
                }
                '"' => quoted = false,
                // A line break inside a quoted field arrives as CRLF.
                '\r' => {}
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            '\r' => {}
            '\n' => {
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            c if c == delimiter => row.push(std::mem::take(&mut field)),
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows.retain(|r| r.iter().any(|f| !f.trim().is_empty()));
    rows
}

/// What a CSV column holds, read off its header. Outlook, Thunderbird and
/// Google each name their columns differently, and each localizes them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Column {
    Email,
    Display,
    Given,
    Family,
    Company,
    Phone,
    Note,
    Other,
}

fn classify(header: &str) -> Column {
    let h = header.trim().to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| h.contains(w));
    let is = |words: &[&str]| words.iter().any(|w| h == *w);

    // "E-mail Type", "E-mail Display Name", "E-mail 1 - Label" sit beside the
    // address columns and are not addresses.
    if has(&["mail", "郵件", "邮件", "邮箱", "信箱", "surel"]) {
        return if has(&[
            "type", "display", "label", "類型", "类型", "顯示", "显示", "jenis",
        ]) {
            Column::Other
        } else {
            Column::Email
        };
    }
    if is(&[
        "name",
        "display name",
        "full name",
        "顯示名稱",
        "显示名称",
        "姓名",
        "全名",
        "名稱",
        "nama",
        "nama tampilan",
        "nama lengkap",
    ]) {
        return Column::Display;
    }
    if is(&["first name", "given name", "名字", "名", "nama depan"]) {
        return Column::Given;
    }
    if is(&[
        "last name",
        "family name",
        "surname",
        "姓氏",
        "姓",
        "nama belakang",
    ]) {
        return Column::Family;
    }
    if has(&[
        "company",
        "organization",
        "organisation",
        "公司",
        "組織",
        "组织",
        "機構",
        "perusahaan",
        "organisasi",
    ]) && !has(&["phone", "電話", "电话", "address", "地址", "telepon"])
    {
        return Column::Company;
    }
    if has(&[
        "phone", "mobile", "電話", "电话", "手機", "手机", "telepon", "ponsel",
    ]) && !has(&["type", "label", "類型", "类型", "fax", "傳真", "传真"])
    {
        return Column::Phone;
    }
    if is(&["notes", "note", "記事", "附註", "备注", "備註", "catatan"]) {
        return Column::Note;
    }
    Column::Other
}

fn parse_csv(text: &str) -> (Vec<Imported>, usize) {
    let first_line = text.lines().next().unwrap_or_default();
    let delimiter = [',', ';', '\t']
        .into_iter()
        .max_by_key(|d| first_line.matches(*d).count())
        .unwrap_or(',');
    let mut rows = csv_rows(text, delimiter).into_iter();
    let Some(header) = rows.next() else {
        return (Vec::new(), 0);
    };
    let columns: Vec<Column> = header.iter().map(|h| classify(h)).collect();
    if !columns.contains(&Column::Email) {
        return (Vec::new(), 0);
    }

    let mut out = Vec::new();
    let mut skipped = 0;
    for row in rows {
        let cells = |kind: Column| {
            columns
                .iter()
                .zip(&row)
                .filter(move |(c, _)| **c == kind)
                .map(|(_, v)| v.trim())
                .filter(|v| !v.is_empty())
        };
        let first = |kind: Column| cells(kind).next().map(str::to_string);
        let display = first(Column::Display).unwrap_or_default();
        let name = if display.is_empty() {
            full_name(
                &first(Column::Given).unwrap_or_default(),
                &first(Column::Family).unwrap_or_default(),
            )
        } else {
            display
        };
        let emails: Vec<String> = cells(Column::Email)
            .filter(|e| valid_email(e))
            .map(str::to_string)
            .collect();
        if emails.is_empty() {
            skipped += 1;
        }
        for email in emails {
            out.push(Imported {
                contact: Contact {
                    id: 0,
                    name: name.clone(),
                    email,
                    company: first(Column::Company),
                    phone: first(Column::Phone),
                    note: first(Column::Note),
                },
                groups: Vec::new(),
            });
        }
    }
    (out, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    fn db() -> Db {
        let db = Db::open_in_memory().unwrap();
        db.with(|conn| ensure_schema(conn)).unwrap();
        db
    }

    fn contact(name: &str, email: &str) -> Contact {
        Contact {
            name: name.into(),
            email: email.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_schema_can_be_created_twice() {
        let db = db();
        db.with(|conn| ensure_schema(conn)).unwrap();
    }

    #[test]
    fn addresses_are_checked_before_they_are_kept() {
        for good in [
            "a@b.co",
            "王小明@example.com.tw",
            " first.last+tag@sub.example.org ",
        ] {
            assert!(valid_email(good), "{good}");
        }
        for bad in [
            "",
            "plain",
            "a@b",
            "@b.co",
            "a@",
            "a b@c.de",
            "a@b@c.de",
            "Ann <a@b.co>",
            "a@.co",
        ] {
            assert!(!valid_email(bad), "{bad}");
        }
    }

    #[test]
    fn a_contact_is_added_edited_and_deleted() {
        let db = db();
        db.with(|conn| {
            let (ann, outcome) = save(conn, &contact("Ann", "ann@example.com"))?;
            assert_eq!(outcome, Saved::Added);
            assert!(ann.id > 0);

            let edited = Contact {
                name: "Ann Lee".into(),
                company: Some("  Acme  ".into()),
                phone: Some("   ".into()),
                ..ann.clone()
            };
            let (stored, outcome) = save(conn, &edited)?;
            assert_eq!(outcome, Saved::Updated);
            assert_eq!(stored.name, "Ann Lee");
            assert_eq!(stored.company.as_deref(), Some("Acme"));
            assert_eq!(stored.phone, None, "blank is stored as nothing");

            delete(conn, &[ann.id])?;
            assert!(list(conn)?.is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_same_address_is_one_contact_and_keeps_what_it_knew() {
        let db = db();
        db.with(|conn| {
            save(
                conn,
                &Contact {
                    company: Some("Acme".into()),
                    ..contact("Ann", "ann@example.com")
                },
            )?;
            // Same person, different capitalization, no name or company.
            let (merged, outcome) = save(conn, &contact("", "ANN@Example.com"))?;
            assert_eq!(outcome, Saved::Updated);
            assert_eq!(merged.name, "Ann");
            assert_eq!(merged.company.as_deref(), Some("Acme"));
            assert_eq!(list(conn)?.len(), 1);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn the_list_is_sorted_by_name_then_address() {
        let db = db();
        db.with(|conn| {
            save(conn, &contact("Zoe", "a@example.com"))?;
            save(conn, &contact("", "m@example.com"))?;
            save(conn, &contact("bob", "z@example.com"))?;
            let order: Vec<String> = list(conn)?.into_iter().map(|c| c.email).collect();
            assert_eq!(order, ["z@example.com", "m@example.com", "a@example.com"]);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn a_group_holds_members_and_loses_deleted_ones() {
        let db = db();
        db.with(|conn| {
            let (ann, _) = save(conn, &contact("Ann", "ann@example.com"))?;
            let (bob, _) = save(conn, &contact("Bob", "bob@example.com"))?;
            let group = save_group(conn, 0, " Sales ", &[ann.id, bob.id, 9999])?;
            assert_eq!(group.name, "Sales");
            assert_eq!(group.member_ids.len(), 2, "an unknown id is ignored");

            let renamed = save_group(conn, group.id, "Sales team", &[bob.id])?;
            assert_eq!(renamed.id, group.id);
            assert_eq!(renamed.member_ids, [bob.id]);

            delete(conn, &[bob.id])?;
            assert!(list_groups(conn)?[0].member_ids.is_empty());

            delete_group(conn, group.id)?;
            assert!(list_groups(conn)?.is_empty());
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn suggestions_offer_the_group_first_then_people() {
        let db = db();
        db.with(|conn| {
            let (ann, _) = save(conn, &contact("Sally Ann", "ann@example.com"))?;
            let (bob, _) = save(conn, &contact("Bob", "bob@sales.example.com"))?;
            save(
                conn,
                &Contact {
                    company: Some("Salesforce".into()),
                    ..contact("Cy", "cy@example.com")
                },
            )?;
            save_group(conn, 0, "Sales", &[ann.id, bob.id])?;
            save_group(conn, 0, "Sales (empty)", &[])?;

            let found = suggest(conn, "%sal%", 8)?;
            assert_eq!(found[0].name.as_deref(), Some("Sales"));
            let mut members: Vec<&str> = found[0].addr.split(", ").collect();
            members.sort();
            assert_eq!(members, ["ann@example.com", "bob@sales.example.com"]);
            // The empty group has nobody to write to; name, address and company
            // all match a person.
            let people: Vec<&str> = found[1..].iter().map(|s| s.addr.as_str()).collect();
            assert_eq!(
                people,
                ["bob@sales.example.com", "cy@example.com", "ann@example.com"]
            );

            assert_eq!(suggest(conn, "%nobody%", 8)?, []);
            assert_eq!(suggest(conn, "%example%", 2)?.len(), 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn reads_a_vcard_3_file() {
        let text = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ann Lee\r\nN:Lee;Ann;;;\r\n\
EMAIL;TYPE=INTERNET:ann@example.com\r\nEMAIL;TYPE=HOME:ann@home.example\r\n\
ORG:Acme\\, Inc.;Sales\r\nTEL;TYPE=CELL:+886 912 345 678\r\nTEL;TYPE=WORK:02-1234\r\n\
NOTE:Met at the fair\\nCall back\r\nCATEGORIES:Sales,VIP\r\nEND:VCARD\r\n";
        let (found, skipped) = parse_file(text);
        assert_eq!(skipped, 0);
        assert_eq!(found.len(), 2, "one contact per address");
        let c = &found[0].contact;
        assert_eq!(c.name, "Ann Lee");
        assert_eq!(c.email, "ann@example.com");
        assert_eq!(c.company.as_deref(), Some("Acme, Inc."));
        assert_eq!(c.phone.as_deref(), Some("+886 912 345 678"));
        assert_eq!(c.note.as_deref(), Some("Met at the fair\nCall back"));
        assert_eq!(found[0].groups, ["Sales", "VIP"]);
        assert_eq!(found[1].contact.email, "ann@home.example");
    }

    #[test]
    fn reads_an_outlook_vcard_2_1_with_quoted_printable() {
        // "王小明" in UTF-8, quoted-printable, with a soft line break.
        let text = "BEGIN:VCARD\r\nVERSION:2.1\r\n\
N;CHARSET=UTF-8;ENCODING=QUOTED-PRINTABLE:=E7=8E=8B;=E5=B0=8F=\r\n=E6=98=8E\r\n\
EMAIL;PREF;INTERNET:ming@example.com\r\nEND:VCARD\r\n";
        let (found, _) = parse_file(text);
        assert_eq!(found.len(), 1);
        assert_eq!(
            found[0].contact.name, "王小明",
            "family name first, no space"
        );
    }

    #[test]
    fn folded_lines_and_grouped_properties_are_understood() {
        let text = "begin:vcard\nFN:A very long\n  name\nitem1.EMAIL;type=INTERNET:a@example.com\nend:vcard\n";
        let (found, _) = parse_file(text);
        assert_eq!(found[0].contact.name, "A very long name");
        assert_eq!(found[0].contact.email, "a@example.com");
    }

    #[test]
    fn a_card_without_an_address_is_counted_not_kept() {
        let text = "BEGIN:VCARD\nFN:No Mail\nTEL:123\nEND:VCARD\nBEGIN:VCARD\nFN:Bad\nEMAIL:not-an-address\nEND:VCARD\n";
        let (found, skipped) = parse_file(text);
        assert!(found.is_empty());
        assert_eq!(skipped, 2);
    }

    #[test]
    fn reads_an_outlook_csv() {
        let text = "\"First Name\",\"Middle Name\",\"Last Name\",\"Company\",\"Business Phone\",\"Mobile Phone\",\
\"E-mail Address\",\"E-mail Type\",\"E-mail Display Name\",\"E-mail 2 Address\",\"Notes\"\r\n\
\"Ann\",\"\",\"Lee\",\"Acme, Inc.\",\"\",\"0912 345 678\",\"ann@example.com\",\"SMTP\",\"Ann Lee (ann@example.com)\",\"ann2@example.com\",\"line one\r\nline two\"\r\n\
\"No\",\"\",\"Mail\",\"\",\"\",\"\",\"\",\"\",\"\",\"\",\"\"\r\n";
        let (found, skipped) = parse_file(text);
        assert_eq!(skipped, 1);
        assert_eq!(found.len(), 2);
        let c = &found[0].contact;
        assert_eq!(c.name, "Ann Lee");
        assert_eq!(c.email, "ann@example.com");
        assert_eq!(c.company.as_deref(), Some("Acme, Inc."));
        assert_eq!(c.phone.as_deref(), Some("0912 345 678"));
        assert_eq!(c.note.as_deref(), Some("line one\nline two"));
        assert_eq!(found[1].contact.email, "ann2@example.com");
    }

    #[test]
    fn reads_a_traditional_chinese_outlook_csv() {
        let text = "名字,姓氏,公司,電子郵件地址,電子郵件類型,行動電話,記事\n小明,王,台灣公司,ming@example.com.tw,SMTP,0912345678,老客戶\n";
        let (found, _) = parse_file(text);
        let c = &found[0].contact;
        assert_eq!(c.name, "王小明");
        assert_eq!(c.email, "ming@example.com.tw");
        assert_eq!(c.company.as_deref(), Some("台灣公司"));
        assert_eq!(c.phone.as_deref(), Some("0912345678"));
        assert_eq!(c.note.as_deref(), Some("老客戶"));
    }

    #[test]
    fn reads_a_thunderbird_csv_and_a_semicolon_one() {
        let text = "First Name,Last Name,Display Name,Nickname,Primary Email,Secondary Email,Work Phone,Organization,Notes\n\
Budi,Santoso,Pak Budi,,budi@example.co.id,,021-555,PT Maju,\n";
        let (found, _) = parse_file(text);
        let c = &found[0].contact;
        assert_eq!(c.name, "Pak Budi", "the display name wins");
        assert_eq!(c.email, "budi@example.co.id");
        assert_eq!(c.company.as_deref(), Some("PT Maju"));
        assert_eq!(c.phone.as_deref(), Some("021-555"));

        let text = "Name;E-mail 1 - Value;Phone 1 - Value\nCy;cy@example.com;555\n";
        let (found, _) = parse_file(text);
        assert_eq!(found[0].contact.name, "Cy");
        assert_eq!(found[0].contact.phone.as_deref(), Some("555"));
    }

    #[test]
    fn a_csv_with_no_address_column_yields_nothing() {
        let (found, skipped) = parse_file("Name,Phone\nAnn,123\n");
        assert!(found.is_empty());
        assert_eq!(skipped, 0);
        assert_eq!(parse_file(""), (Vec::new(), 0));
    }

    #[test]
    fn files_are_decoded_by_bom_then_utf8_then_the_legacy_code_page() {
        let utf16: Vec<u8> = [0xFF, 0xFE]
            .into_iter()
            .chain("名字".encode_utf16().flat_map(u16::to_le_bytes))
            .collect();
        assert_eq!(decode_file(&utf16, encoding_rs::WINDOWS_1252), "名字");
        assert_eq!(decode_file(b"\xEF\xBB\xBFName", encoding_rs::BIG5), "Name");
        assert_eq!(decode_file("王".as_bytes(), encoding_rs::BIG5), "王");
        // "報告" in Big5 is not valid UTF-8.
        assert_eq!(
            decode_file(&[0xB3, 0xF8, 0xA7, 0x69], legacy_encoding("zh-TW")),
            "報告"
        );
        assert_eq!(legacy_encoding("id"), encoding_rs::WINDOWS_1252);
    }

    #[test]
    fn importing_merges_counts_and_builds_groups() {
        let db = db();
        db.with(|conn| {
            save(conn, &contact("Ann", "ann@example.com"))?;
            let entries = vec![
                Imported {
                    contact: contact("Ann L.", "ANN@example.com"),
                    groups: vec!["Sales".into()],
                },
                Imported {
                    contact: contact("Bob", "bob@example.com"),
                    groups: vec!["Sales".into(), "VIP".into()],
                },
            ];
            let report = import(conn, &entries, 3)?;
            assert_eq!(
                report,
                ImportReport {
                    added: 1,
                    updated: 1,
                    skipped: 3
                }
            );
            assert_eq!(list(conn)?.len(), 2);
            let groups = list_groups(conn)?;
            assert_eq!(groups.len(), 2);
            assert_eq!(groups[0].name, "Sales");
            assert_eq!(groups[0].member_ids.len(), 2);
            // Importing the same file again changes nothing but the counts.
            let again = import(conn, &entries, 0)?;
            assert_eq!((again.added, again.updated), (0, 2));
            assert_eq!(list_groups(conn)?[0].member_ids.len(), 2);
            Ok(())
        })
        .unwrap();
    }

    #[test]
    fn an_export_reads_back_as_the_same_book() {
        let db = db();
        db.with(|conn| {
            let (ann, _) = save(
                conn,
                &Contact {
                    company: Some("Acme; Inc, Ltd".into()),
                    phone: Some("+886 2 1234".into()),
                    note: Some("two\nlines".into()),
                    ..contact("王小明", "ming@example.com")
                },
            )?;
            save(conn, &contact("", "bare@example.com"))?;
            save_group(conn, 0, "業務, 一部", &[ann.id])?;

            let text = export_vcards(&list(conn)?, &list_groups(conn)?);
            let (found, skipped) = parse_file(&text);
            assert_eq!(skipped, 0);
            assert_eq!(found.len(), 2);
            let bare = found
                .iter()
                .find(|f| f.contact.email == "bare@example.com")
                .unwrap();
            assert_eq!(bare.contact.name, "", "no name is not turned into one");
            let ming = found
                .iter()
                .find(|f| f.contact.email == "ming@example.com")
                .unwrap();
            assert_eq!(ming.contact.name, "王小明");
            assert_eq!(ming.contact.company.as_deref(), Some("Acme; Inc, Ltd"));
            assert_eq!(ming.contact.phone.as_deref(), Some("+886 2 1234"));
            assert_eq!(ming.contact.note.as_deref(), Some("two\nlines"));
            assert_eq!(ming.groups, ["業務, 一部"]);
            Ok(())
        })
        .unwrap();
    }
}
