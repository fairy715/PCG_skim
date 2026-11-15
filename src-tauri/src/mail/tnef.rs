//! TNEF (`winmail.dat`) unpacking.
//!
//! Fork addition. Outlook, when it believes the recipient is also Outlook,
//! wraps a message's attachments (and its rich-text formatting) into a single
//! `application/ms-tnef` part, usually named `winmail.dat`. Every other mail
//! client then shows one opaque file where the sender attached three. This
//! module opens that wrapper so the files inside can be listed as the
//! attachments they are.
//!
//! Scope: attachment names, content and content ids, plus the plain-text or
//! HTML body when the wrapper carries one. Compressed RTF bodies are not
//! decoded — Outlook sends a plain-text alternative next to the wrapper, and
//! that is what the reader already sees.
//!
//! The input is attacker-controlled, so every read is bounds-checked and the
//! parser returns what it has instead of panicking: a truncated or malformed
//! stream yields the attachments that were complete.

const SIGNATURE: u32 = 0x223E_9F78;

// Attribute ids (low word of the 32-bit attribute tag).
const ATT_BODY: u16 = 0x800C;
const ATT_ATTACH_DATA: u16 = 0x800F;
const ATT_ATTACH_TITLE: u16 = 0x8010;
const ATT_ATTACH_RENDDATA: u16 = 0x9002;
const ATT_MAPI_PROPS: u16 = 0x9003;
const ATT_ATTACHMENT: u16 = 0x9005;
const ATT_OEM_CODEPAGE: u16 = 0x9007;

const LVL_MESSAGE: u8 = 1;
const LVL_ATTACHMENT: u8 = 2;

// MAPI property ids.
const PR_BODY: u16 = 0x1000;
const PR_BODY_HTML: u16 = 0x1013;
const PR_ATTACH_DATA: u16 = 0x3701;
const PR_ATTACH_FILENAME: u16 = 0x3704;
const PR_ATTACH_LONG_FILENAME: u16 = 0x3707;
const PR_ATTACH_MIME_TAG: u16 = 0x370E;
const PR_ATTACH_CONTENT_ID: u16 = 0x3712;

// MAPI property types.
const PT_OBJECT: u16 = 0x000D;
const PT_STRING8: u16 = 0x001E;
const PT_UNICODE: u16 = 0x001F;
const PT_BINARY: u16 = 0x0102;
const MV_FLAG: u16 = 0x1000;

/// Embedded messages are themselves TNEF; this bounds how deep we follow them.
const MAX_DEPTH: u8 = 3;
/// More attachments than this in one wrapper is not mail, it is an attack.
const MAX_ATTACHMENTS: usize = 500;

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TnefAttachment {
    pub filename: Option<String>,
    pub mime_type: Option<String>,
    pub content_id: Option<String>,
    pub data: Vec<u8>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Tnef {
    pub attachments: Vec<TnefAttachment>,
    pub text: Option<String>,
    pub html: Option<String>,
}

/// Cheap test for "this is a TNEF stream", by its 4-byte signature.
pub fn is_tnef(data: &[u8]) -> bool {
    data.len() >= 6 && u32::from_le_bytes([data[0], data[1], data[2], data[3]]) == SIGNATURE
}

/// Unpack a TNEF stream. `None` when the data is not TNEF at all.
pub fn decode(data: &[u8]) -> Option<Tnef> {
    decode_at(data, 0)
}

struct Reader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(n)?;
        let slice = self.data.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        self.take(4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    /// Skip the padding that aligns variable-length MAPI values to 4 bytes.
    fn skip_padding(&mut self, len: usize) -> Option<()> {
        self.take((4 - len % 4) % 4).map(|_| ())
    }
}

fn decode_at(data: &[u8], depth: u8) -> Option<Tnef> {
    if !is_tnef(data) {
        return None;
    }
    let mut r = Reader::new(data);
    r.u32()?; // signature
    r.u16()?; // key

    let mut out = Tnef::default();
    let mut codepage: u32 = 1252;
    // The attachment being assembled; RENDDATA opens a new one.
    let mut current: Option<TnefAttachment> = None;
    // Title from the legacy attribute — the MAPI long filename, when present,
    // is the real name and wins.
    let mut legacy_title: Option<String> = None;
    // The attachment in hand was an attached message whose own files were
    // already listed; the shell around them is not a file of its own.
    let mut expanded = false;
    // Content for the attachment in hand actually arrived — possibly empty.
    // A name whose content never came (a cut-short stream) is not a file.
    let mut has_data = false;

    // Every `?`-like exit below is a `break`: whatever was complete is kept.
    while let Some(level) = r.u8() {
        let Some(tag) = r.u32() else { break };
        let Some(len) = r.u32() else { break };
        let Some(value) = r.take(len as usize) else {
            break;
        };
        if r.u16().is_none() {
            // Missing checksum on the last attribute: the value itself is whole.
        }
        let id = (tag & 0xFFFF) as u16;

        match (level, id) {
            (LVL_MESSAGE, ATT_OEM_CODEPAGE) => {
                if let Some(cp) = Reader::new(value).u32() {
                    codepage = cp;
                }
            }
            (LVL_MESSAGE, ATT_BODY) => {
                let text = decode_string8(value, codepage);
                if !text.trim().is_empty() {
                    out.text.get_or_insert(text);
                }
            }
            (LVL_MESSAGE, ATT_MAPI_PROPS) => {
                for prop in parse_props(value) {
                    match (prop.id, &prop.value) {
                        (PR_BODY, PropValue::Text(t)) if !t.trim().is_empty() => {
                            out.text.get_or_insert_with(|| t.clone());
                        }
                        (PR_BODY, PropValue::Bytes(b)) => {
                            let t = decode_string8(b, codepage);
                            if !t.trim().is_empty() {
                                out.text.get_or_insert(t);
                            }
                        }
                        (PR_BODY_HTML, PropValue::Text(t)) if !t.trim().is_empty() => {
                            out.html.get_or_insert_with(|| t.clone());
                        }
                        (PR_BODY_HTML, PropValue::Bytes(b)) => {
                            let t = decode_string8(b, codepage);
                            if !t.trim().is_empty() {
                                out.html.get_or_insert(t);
                            }
                        }
                        _ => {}
                    }
                }
            }
            (LVL_ATTACHMENT, ATT_ATTACH_RENDDATA) => {
                finish(
                    &mut out,
                    current.take(),
                    legacy_title.take(),
                    expanded,
                    has_data,
                );
                expanded = false;
                has_data = false;
                current = Some(TnefAttachment::default());
            }
            (LVL_ATTACHMENT, ATT_ATTACH_TITLE) => {
                legacy_title = Some(decode_string8(value, codepage));
                current.get_or_insert_with(TnefAttachment::default);
            }
            (LVL_ATTACHMENT, ATT_ATTACH_DATA) => {
                current.get_or_insert_with(TnefAttachment::default).data = value.to_vec();
                has_data = true;
            }
            (LVL_ATTACHMENT, ATT_ATTACHMENT) => {
                let att = current.get_or_insert_with(TnefAttachment::default);
                let mut short_name = None;
                for prop in parse_props(value) {
                    match (prop.id, prop.value) {
                        (PR_ATTACH_LONG_FILENAME, v) => att.filename = v.into_text(codepage),
                        (PR_ATTACH_FILENAME, v) => short_name = v.into_text(codepage),
                        (PR_ATTACH_MIME_TAG, v) => att.mime_type = v.into_text(codepage),
                        (PR_ATTACH_CONTENT_ID, v) => att.content_id = v.into_text(codepage),
                        (PR_ATTACH_DATA, PropValue::Bytes(b)) if att.data.is_empty() => {
                            att.data = b;
                            has_data = true;
                        }
                        (PR_ATTACH_DATA, PropValue::Object(b)) => {
                            // An attached message is itself TNEF, and its own
                            // files are what the reader is after. Any other
                            // object — an embedded Word or Excel document is an
                            // OLE compound file — is kept as the file it is.
                            let inner = (depth < MAX_DEPTH)
                                .then(|| decode_at(&b, depth + 1))
                                .flatten();
                            match inner {
                                Some(inner) => {
                                    out.attachments.extend(inner.attachments);
                                    expanded = true;
                                }
                                None if att.data.is_empty() => {
                                    att.data = b;
                                    has_data = true;
                                }
                                None => {}
                            }
                        }
                        _ => {}
                    }
                }
                if att.filename.is_none() {
                    att.filename = short_name;
                }
            }
            _ => {}
        }
        if out.attachments.len() > MAX_ATTACHMENTS {
            break;
        }
    }
    finish(&mut out, current, legacy_title, expanded, has_data);
    out.attachments.truncate(MAX_ATTACHMENTS);
    Some(out)
}

/// Close the attachment being assembled and keep it if it carries content.
fn finish(
    out: &mut Tnef,
    current: Option<TnefAttachment>,
    legacy_title: Option<String>,
    expanded: bool,
    has_data: bool,
) {
    let Some(mut att) = current else { return };
    if att.filename.as_deref().is_none_or(|n| n.trim().is_empty()) {
        att.filename = legacy_title;
    }
    att.filename = att
        .filename
        .map(|n| base_name(&n))
        .filter(|n| !n.is_empty());
    att.mime_type = att.mime_type.filter(|m| !m.trim().is_empty());
    att.content_id = att
        .content_id
        .map(|c| c.trim_matches(['<', '>', ' ']).to_string())
        .filter(|c| !c.is_empty());
    // A file the sender attached is listed even when it is empty — Outlook
    // writes zero-byte files with their name like any other. A name whose
    // content never arrived is a cut-short stream, not an empty file; an
    // entry with no name and no bytes is a placeholder; and the shell of an
    // attached message whose files were listed is not a file at all.
    let empty_file = has_data && att.filename.is_some();
    if !expanded && (!att.data.is_empty() || empty_file) {
        out.attachments.push(att);
    }
}

/// The file name without any directory the sender's path carried.
fn base_name(name: &str) -> String {
    name.rsplit(['/', '\\'])
        .next()
        .unwrap_or(name)
        .trim()
        .to_string()
}

enum PropValue {
    Text(String),
    Bytes(Vec<u8>),
    /// `PT_OBJECT` content with its 16-byte interface id already removed.
    Object(Vec<u8>),
}

impl PropValue {
    fn into_text(self, codepage: u32) -> Option<String> {
        let text = match self {
            PropValue::Text(t) => t,
            PropValue::Bytes(b) => decode_string8(&b, codepage),
            PropValue::Object(_) => return None,
        };
        Some(text).filter(|t| !t.trim().is_empty())
    }
}

struct Prop {
    id: u16,
    value: PropValue,
}

/// Parse a MAPI property block, returning the string/binary properties and
/// skipping over everything else. Stops at the first value it cannot size.
fn parse_props(data: &[u8]) -> Vec<Prop> {
    let mut props = Vec::new();
    let mut r = Reader::new(data);
    let Some(count) = r.u32() else {
        return props;
    };
    for _ in 0..count {
        if parse_prop(&mut r, &mut props).is_none() {
            break;
        }
    }
    props
}

fn parse_prop(r: &mut Reader<'_>, props: &mut Vec<Prop>) -> Option<()> {
    let raw_type = r.u16()?;
    let id = r.u16()?;
    // Named properties carry their name inline, before the value.
    if id >= 0x8000 {
        r.take(16)?; // property set GUID
        let kind = r.u32()?;
        if kind == 0 {
            r.u32()?; // numeric name
        } else {
            let len = r.u32()? as usize;
            r.take(len)?;
            r.skip_padding(len)?;
        }
    }
    let multi = raw_type & MV_FLAG != 0;
    let ty = raw_type & !MV_FLAG;
    match ty {
        PT_STRING8 | PT_UNICODE | PT_BINARY | PT_OBJECT => {
            // Variable-length values always carry a count, even when single.
            let values = r.u32()?;
            for i in 0..values {
                let len = r.u32()? as usize;
                let bytes = r.take(len)?;
                r.skip_padding(len)?;
                // Only the first value of a multi-valued property is of use.
                if i > 0 {
                    continue;
                }
                let value = match ty {
                    PT_UNICODE => PropValue::Text(decode_utf16(bytes)),
                    PT_OBJECT => PropValue::Object(bytes.get(16..).unwrap_or_default().to_vec()),
                    _ => PropValue::Bytes(bytes.to_vec()),
                };
                props.push(Prop { id, value });
            }
        }
        _ => {
            let width: usize = match ty {
                // Short, long, float, error, boolean: padded to 4 bytes.
                0x0002 | 0x0003 | 0x0004 | 0x000A | 0x000B => 4,
                // Double, currency, application time, int64, file time.
                0x0005 | 0x0006 | 0x0007 | 0x0014 | 0x0040 => 8,
                // GUID.
                0x0048 => 16,
                // Unknown type: its size is unknowable, so stop here.
                _ => return None,
            };
            let values = if multi { r.u32()? as usize } else { 1 };
            r.take(width.checked_mul(values)?)?;
        }
    }
    Some(())
}

fn decode_utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

/// Decode an 8-bit string: UTF-8 when it is valid UTF-8 (which covers plain
/// ASCII), otherwise the wrapper's declared Windows code page.
pub(crate) fn decode_string8(bytes: &[u8], codepage: u32) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    let bytes = &bytes[..end];
    if let Ok(s) = std::str::from_utf8(bytes) {
        return s.to_string();
    }
    let (text, _, _) = encoding_for(codepage).decode(bytes);
    text.into_owned()
}

fn encoding_for(codepage: u32) -> &'static encoding_rs::Encoding {
    use encoding_rs::*;
    match codepage {
        874 => WINDOWS_874,
        932 => SHIFT_JIS,
        936 => GBK,
        949 | 51949 => EUC_KR,
        950 => BIG5,
        866 => IBM866,
        20866 => KOI8_R,
        21866 => KOI8_U,
        28592 => ISO_8859_2,
        28595 => ISO_8859_5,
        28597 => ISO_8859_7,
        28605 => ISO_8859_15,
        50220..=50222 => ISO_2022_JP,
        51932 => EUC_JP,
        54936 => GB18030,
        1250 => WINDOWS_1250,
        1251 => WINDOWS_1251,
        1253 => WINDOWS_1253,
        1254 => WINDOWS_1254,
        1255 => WINDOWS_1255,
        1256 => WINDOWS_1256,
        1257 => WINDOWS_1257,
        1258 => WINDOWS_1258,
        65001 => UTF_8,
        _ => WINDOWS_1252,
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! Builders for TNEF streams, shared with the parser's tests.
    use super::*;

    pub fn attribute(level: u8, id: u16, value: &[u8]) -> Vec<u8> {
        let mut out = vec![level];
        out.extend_from_slice(&(0x0006_0000u32 | u32::from(id)).to_le_bytes());
        out.extend_from_slice(&(value.len() as u32).to_le_bytes());
        out.extend_from_slice(value);
        let sum: u32 = value.iter().map(|b| u32::from(*b)).sum();
        out.extend_from_slice(&((sum & 0xFFFF) as u16).to_le_bytes());
        out
    }

    pub fn stream(attributes: &[Vec<u8>]) -> Vec<u8> {
        let mut out = SIGNATURE.to_le_bytes().to_vec();
        out.extend_from_slice(&1u16.to_le_bytes());
        for a in attributes {
            out.extend_from_slice(a);
        }
        out
    }

    fn padded(bytes: &[u8]) -> Vec<u8> {
        let mut out = (bytes.len() as u32).to_le_bytes().to_vec();
        out.extend_from_slice(bytes);
        out.resize(out.len() + (4 - bytes.len() % 4) % 4, 0);
        out
    }

    pub fn prop_unicode(id: u16, text: &str) -> Vec<u8> {
        let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        bytes.extend_from_slice(&[0, 0]);
        let mut out = PT_UNICODE.to_le_bytes().to_vec();
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend(padded(&bytes));
        out
    }

    pub fn prop_binary(ty: u16, id: u16, bytes: &[u8]) -> Vec<u8> {
        let mut out = ty.to_le_bytes().to_vec();
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend(padded(bytes));
        out
    }

    pub fn prop_long(id: u16, value: u32) -> Vec<u8> {
        let mut out = 0x0003u16.to_le_bytes().to_vec();
        out.extend_from_slice(&id.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
        out
    }

    pub fn props(list: &[Vec<u8>]) -> Vec<u8> {
        let mut out = (list.len() as u32).to_le_bytes().to_vec();
        for p in list {
            out.extend_from_slice(p);
        }
        out
    }

    /// A wrapper holding one file, the way Outlook writes it.
    pub fn one_file(name: &str, content: &[u8]) -> Vec<u8> {
        stream(&[
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_DATA, content),
            attribute(
                LVL_ATTACHMENT,
                ATT_ATTACHMENT,
                &props(&[prop_unicode(PR_ATTACH_LONG_FILENAME, name)]),
            ),
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    #[test]
    fn anything_without_the_signature_is_not_tnef() {
        assert!(decode(b"").is_none());
        assert!(decode(b"%PDF-1.7 not a wrapper").is_none());
        assert!(!is_tnef(&SIGNATURE.to_le_bytes())); // signature but no key
    }

    #[test]
    fn an_empty_wrapper_holds_nothing() {
        let t = decode(&stream(&[])).unwrap();
        assert_eq!(t, Tnef::default());
    }

    #[test]
    fn unpacks_a_file_with_its_long_name() {
        let t = decode(&one_file("Quarterly report.xlsx", b"spreadsheet bytes")).unwrap();
        assert_eq!(t.attachments.len(), 1);
        assert_eq!(
            t.attachments[0].filename.as_deref(),
            Some("Quarterly report.xlsx")
        );
        assert_eq!(t.attachments[0].data, b"spreadsheet bytes");
    }

    #[test]
    fn several_files_stay_separate_and_in_order() {
        let data = stream(&[
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_TITLE, b"a.txt\0"),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_DATA, b"first"),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_TITLE, b"b.txt\0"),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_DATA, b"second"),
        ]);
        let t = decode(&data).unwrap();
        let names: Vec<_> = t
            .attachments
            .iter()
            .map(|a| a.filename.clone().unwrap())
            .collect();
        assert_eq!(names, ["a.txt", "b.txt"]);
        assert_eq!(t.attachments[1].data, b"second");
    }

    #[test]
    fn the_long_name_wins_over_the_8_3_title() {
        let data = stream(&[
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_TITLE, b"QUARTE~1.XLS\0"),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_DATA, b"x"),
            attribute(
                LVL_ATTACHMENT,
                ATT_ATTACHMENT,
                &props(&[
                    prop_long(0x3705, 1), // attach method: skipped over
                    prop_unicode(PR_ATTACH_LONG_FILENAME, "Quarterly report.xlsx"),
                    prop_unicode(PR_ATTACH_MIME_TAG, "application/vnd.ms-excel"),
                    prop_unicode(PR_ATTACH_CONTENT_ID, "<img1@host>"),
                ]),
            ),
        ]);
        let a = &decode(&data).unwrap().attachments[0];
        assert_eq!(a.filename.as_deref(), Some("Quarterly report.xlsx"));
        assert_eq!(a.mime_type.as_deref(), Some("application/vnd.ms-excel"));
        assert_eq!(a.content_id.as_deref(), Some("img1@host"));
    }

    #[test]
    fn a_big5_file_name_is_decoded_by_the_declared_code_page() {
        // "報告.txt" in Big5, with the wrapper declaring code page 950.
        let big5 = [0xB3, 0xF8, 0xA7, 0x69, b'.', b't', b'x', b't', 0];
        let mut codepage = 950u32.to_le_bytes().to_vec();
        codepage.extend_from_slice(&0u32.to_le_bytes());
        let data = stream(&[
            attribute(LVL_MESSAGE, ATT_OEM_CODEPAGE, &codepage),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_TITLE, &big5),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_DATA, b"x"),
        ]);
        let a = &decode(&data).unwrap().attachments[0];
        assert_eq!(a.filename.as_deref(), Some("報告.txt"));
    }

    #[test]
    fn a_sender_path_is_reduced_to_the_file_name() {
        let t = decode(&one_file("C:\\Users\\ann\\..\\secret\\plan.docx", b"x")).unwrap();
        assert_eq!(t.attachments[0].filename.as_deref(), Some("plan.docx"));
    }

    #[test]
    fn content_can_arrive_as_a_mapi_property() {
        let data = stream(&[
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(
                LVL_ATTACHMENT,
                ATT_ATTACHMENT,
                &props(&[
                    prop_unicode(PR_ATTACH_LONG_FILENAME, "note.txt"),
                    prop_binary(PT_BINARY, PR_ATTACH_DATA, b"from a property"),
                ]),
            ),
        ]);
        let a = &decode(&data).unwrap().attachments[0];
        assert_eq!(a.data, b"from a property");
    }

    #[test]
    fn files_inside_an_attached_message_are_surfaced() {
        let inner = one_file("inner.pdf", b"pdf bytes");
        let mut object = vec![0u8; 16]; // interface id
        object.extend_from_slice(&inner);
        let data = stream(&[
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(
                LVL_ATTACHMENT,
                ATT_ATTACHMENT,
                &props(&[prop_binary(PT_OBJECT, PR_ATTACH_DATA, &object)]),
            ),
        ]);
        let t = decode(&data).unwrap();
        assert_eq!(t.attachments.len(), 1);
        assert_eq!(t.attachments[0].filename.as_deref(), Some("inner.pdf"));
    }

    /// Outlook embeds a pasted Office document as an OLE storage object
    /// (interface id IStorage, then a compound file). Found in a real sample.
    #[test]
    fn an_embedded_document_is_kept_as_a_file() {
        let mut object = vec![0x0B, 0, 0, 0, 0, 0, 0, 0, 0xC0, 0, 0, 0, 0, 0, 0, 0x46];
        object.extend_from_slice(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 1, 2, 3]);
        let data = stream(&[
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_TITLE, b"Untitled_Attachment\0"),
            attribute(
                LVL_ATTACHMENT,
                ATT_ATTACHMENT,
                &props(&[prop_binary(PT_OBJECT, PR_ATTACH_DATA, &object)]),
            ),
        ]);
        let t = decode(&data).unwrap();
        assert_eq!(t.attachments.len(), 1);
        let a = &t.attachments[0];
        assert_eq!(a.filename.as_deref(), Some("Untitled_Attachment"));
        assert_eq!(
            &a.data[..4],
            &[0xD0, 0xCF, 0x11, 0xE0],
            "the compound file, without the id"
        );
    }

    /// A real sample attaches AUTOEXEC.BAT and CONFIG.SYS with no bytes.
    #[test]
    fn an_empty_file_with_a_name_is_still_listed() {
        let data = stream(&[
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_DATA, b""),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_TITLE, b"CONFIG.SYS\0"),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_DATA, b"[boot loader]"),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_TITLE, b"boot.ini\0"),
        ]);
        let names: Vec<_> = decode(&data)
            .unwrap()
            .attachments
            .into_iter()
            .map(|a| (a.filename.unwrap(), a.data.len()))
            .collect();
        assert_eq!(
            names,
            [("CONFIG.SYS".to_string(), 0), ("boot.ini".to_string(), 13)]
        );
    }

    #[test]
    fn nesting_is_followed_only_so_deep() {
        let mut data = one_file("deep.txt", b"x");
        for _ in 0..10 {
            let mut object = vec![0u8; 16];
            object.extend_from_slice(&data);
            data = stream(&[
                attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
                attribute(
                    LVL_ATTACHMENT,
                    ATT_ATTACHMENT,
                    &props(&[prop_binary(PT_OBJECT, PR_ATTACH_DATA, &object)]),
                ),
            ]);
        }
        // Past the limit the innermost wrapper is not opened; it is kept as
        // an opaque file rather than lost.
        let t = decode(&data).unwrap();
        assert_eq!(t.attachments.len(), 1);
        assert!(is_tnef(&t.attachments[0].data[16..]) || is_tnef(&t.attachments[0].data));
    }

    #[test]
    fn the_body_is_read_when_the_wrapper_carries_one() {
        let data = stream(&[
            attribute(LVL_MESSAGE, ATT_BODY, b"Plain body\0"),
            attribute(
                LVL_MESSAGE,
                ATT_MAPI_PROPS,
                &props(&[prop_binary(PT_BINARY, PR_BODY_HTML, b"<p>Rich body</p>")]),
            ),
        ]);
        let t = decode(&data).unwrap();
        assert_eq!(t.text.as_deref(), Some("Plain body"));
        assert_eq!(t.html.as_deref(), Some("<p>Rich body</p>"));
    }

    #[test]
    fn a_truncated_stream_keeps_what_was_complete() {
        let mut data = stream(&[
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_TITLE, b"whole.txt\0"),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_DATA, b"complete"),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_TITLE, b"cut.txt\0"),
            attribute(LVL_ATTACHMENT, ATT_ATTACH_DATA, b"this one is cut short"),
        ]);
        data.truncate(data.len() - 10);
        let t = decode(&data).unwrap();
        assert_eq!(t.attachments.len(), 1);
        assert_eq!(t.attachments[0].filename.as_deref(), Some("whole.txt"));
    }

    #[test]
    fn a_length_that_overruns_the_stream_does_not_panic() {
        let mut data = stream(&[]);
        data.push(LVL_ATTACHMENT);
        data.extend_from_slice(&(0x0006_0000u32 | u32::from(ATT_ATTACH_DATA)).to_le_bytes());
        data.extend_from_slice(&u32::MAX.to_le_bytes());
        data.extend_from_slice(b"short");
        assert!(decode(&data).unwrap().attachments.is_empty());
    }

    #[test]
    fn garbage_in_a_property_block_is_survived() {
        for seed in 0u8..=255 {
            let junk: Vec<u8> = (0..64u8)
                .map(|i| seed.wrapping_mul(31).wrapping_add(i.wrapping_mul(17)))
                .collect();
            let data = stream(&[
                attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14]),
                attribute(LVL_ATTACHMENT, ATT_ATTACHMENT, &junk),
                attribute(LVL_MESSAGE, ATT_MAPI_PROPS, &junk),
            ]);
            let _ = decode(&data);
        }
    }

    /// Manual check against real Outlook-written files:
    /// `SKIM_TNEF_SAMPLES=<dir> cargo test --lib -- --ignored real_tnef --nocapture`
    #[test]
    #[ignore = "needs SKIM_TNEF_SAMPLES pointing at a folder of winmail.dat files"]
    fn real_tnef_samples() {
        let Ok(dir) = std::env::var("SKIM_TNEF_SAMPLES") else {
            return;
        };
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .filter(|p| matches!(p.extension().and_then(|e| e.to_str()), Some("tnef" | "dat")))
            .collect();
        entries.sort();
        for path in entries {
            let data = std::fs::read(&path).unwrap();
            let name = path.file_name().unwrap().to_string_lossy();
            match decode(&data) {
                None => println!("{name}: NOT TNEF"),
                Some(t) => {
                    let files: Vec<String> = t
                        .attachments
                        .iter()
                        .map(|a| {
                            format!(
                                "{} ({} B{})",
                                a.filename.as_deref().unwrap_or("<no name>"),
                                a.data.len(),
                                a.mime_type
                                    .as_deref()
                                    .map(|m| format!(", {m}"))
                                    .unwrap_or_default()
                            )
                        })
                        .collect();
                    // Where a reference extraction exists, compare the bytes.
                    if let Ok(baselines) = std::env::var("SKIM_TNEF_BASELINES") {
                        for a in &t.attachments {
                            let Some(file) = &a.filename else { continue };
                            let reference =
                                std::path::Path::new(&baselines).join(format!("{file}.baseline"));
                            if let Ok(expected) = std::fs::read(&reference) {
                                // The reference keeps an OLE object's 16-byte id.
                                let same =
                                    expected == a.data || expected.get(16..) == Some(&a.data[..]);
                                println!(
                                    "  {file}: bytes {}",
                                    if same { "match" } else { "DIFFER" }
                                );
                                assert!(same, "{file} differs from the reference");
                            }
                        }
                    }
                    println!(
                        "{name}: [{}] text={} html={}",
                        files.join(" | "),
                        t.text.as_ref().map_or(0, |s| s.len()),
                        t.html.as_ref().map_or(0, |s| s.len())
                    );
                }
            }
        }
    }

    #[test]
    fn an_entry_with_neither_name_nor_content_is_not_listed() {
        let data = stream(&[attribute(LVL_ATTACHMENT, ATT_ATTACH_RENDDATA, &[0; 14])]);
        assert!(decode(&data).unwrap().attachments.is_empty());
    }
}
