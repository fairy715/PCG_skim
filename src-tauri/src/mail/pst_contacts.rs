//! Contacts and distribution lists from an Outlook PST (fork addition).
//!
//! Read by the PST import (see `pst_import`) from every contacts folder in the
//! file and merged into the address book (see `crate::contacts`), where a
//! contact is keyed on its address: importing a file twice changes nothing.
//!
//! Outlook keeps a contact's e-mail addresses in *named* properties
//! (PSETID_Address, "Email1EmailAddress" and so on), whose property ids differ
//! from file to file; they are looked up in the file's own name map first.
//! A distribution list becomes a group. Its members are read from the one-off
//! entry ids Outlook stores for each of them, which carry a name and an
//! address directly.

use crate::contacts::{self, Contact, Imported};
use crate::mail::tnef;
use outlook_pst::ltp::prop_context::{GuidValue, PropertyValue};
use outlook_pst::messaging::folder::Folder;
use outlook_pst::messaging::message::{Message, MessageProperties};
use outlook_pst::messaging::named_prop::{NamedPropertyGuid, NamedPropertyId};
use outlook_pst::messaging::store::Store;
use outlook_pst::ndb::node_id::NodeId;
use std::collections::HashMap;
use std::rc::Rc;

// Tagged properties.
const PR_BODY: u16 = 0x1000;
const PR_DISPLAY_NAME: u16 = 0x3001;
const PR_GIVEN_NAME: u16 = 0x3A06;
const PR_BUSINESS_TELEPHONE: u16 = 0x3A08;
const PR_HOME_TELEPHONE: u16 = 0x3A09;
const PR_SURNAME: u16 = 0x3A11;
const PR_COMPANY_NAME: u16 = 0x3A16;
const PR_PRIMARY_TELEPHONE: u16 = 0x3A1A;
const PR_MOBILE_TELEPHONE: u16 = 0x3A1C;

// PSETID_Address long ids ([MS-OXOCNTC]).
const LID_EMAIL_ADDRESS: [u32; 3] = [0x8083, 0x8093, 0x80A3];
const LID_EMAIL_ORIGINAL_DISPLAY_NAME: [u32; 3] = [0x8084, 0x8094, 0x80A4];
const LID_DISTRIBUTION_LIST_NAME: u32 = 0x8053;
const LID_DISTRIBUTION_LIST_ONE_OFF_MEMBERS: u32 = 0x8054;
const LID_DISTRIBUTION_LIST_MEMBERS: u32 = 0x8055;

/// {00062004-0000-0000-C000-000000000046}
const PSETID_ADDRESS: (u32, u16, u16, [u8; 8]) =
    (0x0006_2004, 0, 0, [0xC0, 0, 0, 0, 0, 0, 0, 0x46]);

/// The provider id that marks a one-off entry id ([MS-OXCDATA] 2.2.5.1).
const ONE_OFF_PROVIDER: [u8; 16] = [
    0x81, 0x2B, 0x1F, 0xA4, 0xBE, 0xA3, 0x10, 0x19, 0x9D, 0x6E, 0x00, 0xDD, 0x01, 0x0F, 0x54, 0x02,
];
const MAPI_UNICODE: u16 = 0x8000;

fn is_psetid_address(g: &GuidValue) -> bool {
    (g.data1(), g.data2(), g.data3(), *g.data4()) == PSETID_ADDRESS
}

/// Where this file keeps the named properties we read.
#[derive(Default)]
struct Names {
    /// PSETID_Address long id → this file's property id.
    address: HashMap<u32, u16>,
    /// PS_PUBLIC_STRINGS "Keywords": the item's categories.
    keywords: Option<u16>,
}

impl Names {
    fn read(store: &dyn Store) -> Self {
        let mut names = Names::default();
        let Ok(map) = store.named_property_map() else {
            return names;
        };
        let props = map.properties();
        let guids = props.stream_guid().unwrap_or_default();
        for entry in props.stream_entry().unwrap_or_default() {
            match (entry.guid(), entry.id()) {
                (NamedPropertyGuid::GuidIndex(i), NamedPropertyId::Number(lid))
                    if guids.get(usize::from(i)).is_some_and(is_psetid_address) =>
                {
                    names.address.insert(lid, entry.prop_id());
                }
                (NamedPropertyGuid::PublicStrings, NamedPropertyId::StringOffset(offset)) => {
                    let name = props
                        .lookup_string(offset)
                        .map(|s| utf16(s.buffer()))
                        .unwrap_or_default();
                    if name.eq_ignore_ascii_case("Keywords") {
                        names.keywords = Some(entry.prop_id());
                    }
                }
                _ => {}
            }
        }
        names
    }

    fn get<'a>(&self, props: &'a MessageProperties, lid: u32) -> Option<&'a PropertyValue> {
        props.get(*self.address.get(&lid)?)
    }
}

fn utf16(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|c| u16::from_le_bytes(*c))
        .take_while(|u| *u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

fn text(value: Option<&PropertyValue>) -> Option<String> {
    let text = match value? {
        PropertyValue::String8(v) => tnef::decode_string8(v.buffer(), 1252),
        PropertyValue::Unicode(v) => v.to_string(),
        _ => return None,
    };
    Some(text.trim_end_matches('\0').trim().to_string()).filter(|t| !t.is_empty())
}

fn texts(value: Option<&PropertyValue>) -> Vec<String> {
    let all: Vec<String> = match value {
        Some(PropertyValue::MultipleUnicode(v)) => v.iter().map(|s| s.to_string()).collect(),
        Some(PropertyValue::MultipleString8(v)) => v
            .iter()
            .map(|s| tnef::decode_string8(s.buffer(), 1252))
            .collect(),
        other => text(other).into_iter().collect(),
    };
    all.into_iter()
        .map(|s| s.trim_end_matches('\0').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// An SMTP address out of what Outlook stored: the address itself, or a
/// display form such as `Ann Lee (ann@example.com)` or `Ann <ann@example.com>`.
/// An Exchange directory name (`/O=ORG/CN=…`) is not something to write to.
fn smtp_address(value: &str) -> Option<String> {
    // "SMTP:" and "mailto:" are prefixes, not part of the address — and a
    // colon is a legal character in one, so they must go before the check.
    let strip = |part: &str| {
        let part = part.trim();
        ["SMTP:", "smtp:", "mailto:", "MAILTO:"]
            .iter()
            .find_map(|p| part.strip_prefix(p))
            .unwrap_or(part)
            .to_string()
    };
    let value = strip(value);
    if contacts::valid_email(&value) {
        return Some(value);
    }
    value
        .split(['<', '>', '(', ')', ' ', '[', ']', ';', ','])
        .map(strip)
        .find(|part| contacts::valid_email(part))
}

/// Name and address out of a one-off entry id. `None` for any other kind.
fn one_off(entry_id: &[u8]) -> Option<(String, String)> {
    if entry_id.get(4..20)? != ONE_OFF_PROVIDER {
        return None;
    }
    let flags = u16::from_le_bytes(entry_id.get(22..24)?.try_into().ok()?);
    let rest = entry_id.get(24..)?;
    // Three NUL-terminated strings: display name, address type, address.
    let fields: Vec<String> = if flags & MAPI_UNICODE != 0 {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| u16::from_le_bytes(*c))
            .collect();
        units
            .split(|u| *u == 0)
            .take(3)
            .map(String::from_utf16_lossy)
            .collect()
    } else {
        rest.split(|b| *b == 0)
            .take(3)
            .map(|b| tnef::decode_string8(b, 1252))
            .collect()
    };
    let [name, _kind, address] = fields.as_slice() else {
        return None;
    };
    let address = smtp_address(address)?;
    let name = if name.trim().eq_ignore_ascii_case(&address) {
        String::new()
    } else {
        name.trim().to_string()
    };
    Some((name, address))
}

/// What a contacts folder held.
#[derive(Debug, Default)]
pub struct Found {
    pub entries: Vec<Imported>,
    /// Contacts with no usable e-mail address.
    pub skipped: usize,
}

fn read_contact(props: &MessageProperties, names: &Names, found: &mut Found) {
    let name = text(props.get(PR_DISPLAY_NAME)).unwrap_or_else(|| {
        contacts::full_name(
            &text(props.get(PR_GIVEN_NAME)).unwrap_or_default(),
            &text(props.get(PR_SURNAME)).unwrap_or_default(),
        )
    });
    let phone = [
        PR_MOBILE_TELEPHONE,
        PR_BUSINESS_TELEPHONE,
        PR_PRIMARY_TELEPHONE,
        PR_HOME_TELEPHONE,
    ]
    .into_iter()
    .find_map(|id| text(props.get(id)));
    let groups = names
        .keywords
        .map(|id| texts(props.get(id)))
        .unwrap_or_default();

    let mut addresses: Vec<String> = Vec::new();
    for (addr_lid, display_lid) in LID_EMAIL_ADDRESS
        .iter()
        .zip(LID_EMAIL_ORIGINAL_DISPLAY_NAME)
    {
        let address = text(names.get(props, *addr_lid))
            .and_then(|a| smtp_address(&a))
            // An Exchange contact's address is a directory name; the original
            // display name often still holds the SMTP form.
            .or_else(|| text(names.get(props, display_lid)).and_then(|d| smtp_address(&d)));
        if let Some(address) = address {
            if !addresses.iter().any(|a| a.eq_ignore_ascii_case(&address)) {
                addresses.push(address);
            }
        }
    }
    if addresses.is_empty() {
        found.skipped += 1;
        return;
    }
    for email in addresses {
        found.entries.push(Imported {
            contact: Contact {
                id: 0,
                name: if name.eq_ignore_ascii_case(&email) {
                    String::new()
                } else {
                    name.clone()
                },
                email,
                company: text(props.get(PR_COMPANY_NAME)),
                phone: phone.clone(),
                note: text(props.get(PR_BODY)),
            },
            groups: groups.clone(),
        });
    }
}

fn read_distribution_list(props: &MessageProperties, names: &Names, found: &mut Found) {
    let Some(list) = text(props.get(PR_DISPLAY_NAME))
        .or_else(|| text(names.get(props, LID_DISTRIBUTION_LIST_NAME)))
    else {
        return;
    };
    let entry_ids = |lid| match names.get(props, lid) {
        Some(PropertyValue::MultipleBinary(ids)) => {
            ids.iter().map(|b| b.buffer().to_vec()).collect()
        }
        _ => Vec::new(),
    };
    // The one-off list has a name and address for every member, contacts in
    // the file included. The member list has one-offs only for people who
    // are not contacts, and is the fallback.
    let mut members: Vec<(String, String)> = entry_ids(LID_DISTRIBUTION_LIST_ONE_OFF_MEMBERS)
        .iter()
        .filter_map(|id| one_off(id))
        .collect();
    if members.is_empty() {
        members = entry_ids(LID_DISTRIBUTION_LIST_MEMBERS)
            .iter()
            .filter_map(|id| one_off(id))
            .collect();
    }
    for (name, email) in members {
        found.entries.push(Imported {
            contact: Contact {
                name,
                email,
                ..Default::default()
            },
            groups: vec![list.clone()],
        });
    }
}

/// Read every contact and distribution list in these contacts folders.
/// An item that cannot be read is passed over; the rest still import.
pub fn read(store: &dyn Store, folders: &[Rc<dyn Folder>]) -> Found {
    let names = Names::read(store);
    let mut found = Found::default();
    for folder in folders {
        let Some(table) = folder.contents_table() else {
            continue;
        };
        for row in table.rows_matrix() {
            let node = NodeId::from(u32::from(row.id()));
            let message = store
                .properties()
                .make_entry_id(node)
                .and_then(|id| store.open_message(&id, None));
            let message: Rc<dyn Message> = match message {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!(node = u32::from(node), error = %e, "cannot read PST contact");
                    continue;
                }
            };
            let props = message.properties();
            let class = props
                .message_class()
                .unwrap_or_default()
                .to_ascii_lowercase();
            if class.starts_with("ipm.distlist") {
                read_distribution_list(props, &names, &mut found);
            } else if class.starts_with("ipm.contact") {
                read_contact(props, &names, &mut found);
            }
        }
    }
    found
}

/// Is this a folder of contacts? Outlook's own contacts folder and any other
/// folder created to hold contacts say so in their container class.
pub fn is_contacts_folder(class: Option<&str>) -> bool {
    class.is_some_and(|c| c.to_ascii_lowercase().starts_with("ipf.contact"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn reads_a_unicode_one_off_entry_id_written_by_outlook() {
        // Taken from a distribution list Outlook wrote.
        let id = hex(
            "00000000812b1fa4bea310199d6e00dd010f540200000180640069007300740020006e0061006d0065\
             002000310000005300\
             4d0054005000000064006900730074003100400072006a006f0068006e0073006f006e002e0069006400\
             2e00610075000000",
        );
        assert_eq!(
            one_off(&id),
            Some(("dist name 1".into(), "dist1@rjohnson.id.au".into()))
        );
    }

    #[test]
    fn reads_an_8_bit_one_off_entry_id() {
        let mut id = vec![0, 0, 0, 0];
        id.extend_from_slice(&ONE_OFF_PROVIDER);
        id.extend_from_slice(&[0, 0, 0, 0]); // version, flags: not Unicode
        id.extend_from_slice(b"Ann Lee\0SMTP\0ann@example.com\0");
        assert_eq!(
            one_off(&id),
            Some(("Ann Lee".into(), "ann@example.com".into()))
        );
    }

    #[test]
    fn other_entry_ids_are_not_one_offs() {
        // A wrapped entry id pointing at a contact in the same file.
        let wrapped = hex("00000000c091add3519dcf11a4a900aa0047faa4c300000000a41d63dbc53b8e4ab8071e15e55750ce64002000");
        assert_eq!(one_off(&wrapped), None);
        assert_eq!(one_off(&[]), None);
        assert_eq!(one_off(&[0; 23]), None);
    }

    #[test]
    fn a_one_off_without_a_usable_address_is_dropped() {
        let mut id = vec![0, 0, 0, 0];
        id.extend_from_slice(&ONE_OFF_PROVIDER);
        id.extend_from_slice(&[0, 0, 0, 0]);
        id.extend_from_slice(b"Boss\0EX\0/O=ORG/OU=SITE/CN=RECIPIENTS/CN=BOSS\0");
        assert_eq!(one_off(&id), None);
    }

    #[test]
    fn addresses_are_found_in_their_display_forms() {
        assert_eq!(
            smtp_address("ann@example.com").as_deref(),
            Some("ann@example.com")
        );
        assert_eq!(
            smtp_address("Ann Lee (ann@example.com)").as_deref(),
            Some("ann@example.com")
        );
        assert_eq!(
            smtp_address("王小明 <ming@example.com.tw>").as_deref(),
            Some("ming@example.com.tw")
        );
        assert_eq!(
            smtp_address("SMTP:ann@example.com").as_deref(),
            Some("ann@example.com")
        );
        assert_eq!(smtp_address("/O=ORG/OU=SITE/CN=RECIPIENTS/CN=ANN"), None);
        assert_eq!(smtp_address("Ann Lee"), None);
    }

    #[test]
    fn contacts_folders_are_told_by_their_class() {
        assert!(is_contacts_folder(Some("IPF.Contact")));
        assert!(is_contacts_folder(Some("IPF.Contact.MOC.QuickContacts")));
        assert!(!is_contacts_folder(Some("IPF.Note")));
        assert!(!is_contacts_folder(None));
    }
}
