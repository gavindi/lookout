//! Outlook's "Comma Separated Values (Windows)" contact file, import and
//! export (OUTLOOK_PARITY.md §4.6, Phase 4 item 4.5 / P-08).
//!
//! The file is RFC 4180 CSV with a header row. Outlook writes its full classic
//! column set (see [`OUTLOOK_CSV_COLUMNS`]) in a fixed order with empty cells
//! for absent values; files from other tools and older Outlooks carry a
//! subset, a different order, or extra columns. Reading is therefore
//! header-driven and tolerant: column names match case- and
//! punctuation-insensitively (`E-mail Address`, `Email address` and
//! `E-MAIL2ADDRESS` all work) in any order, unknown columns are ignored, a
//! missing column simply leaves its field unset, and a UTF-8 BOM plus LF or
//! CRLF line endings are all accepted. A file with no recognisable column is
//! rejected rather than imported as blank cards.
//!
//! Only columns with a [`VCard`] slot are mapped: names, company, job title,
//! notes, web pages, dates, categories, up to three e-mail addresses, the
//! phone set and the home/work/other postal addresses, plus Outlook's
//! `Spouse` column kept as the `X-SPOUSE` custom property. Everything else
//! (initials, gender, language, `User 1`…) is ignored on import and emitted
//! empty on export. Outlook CSV has no vehicle for photos, instant messaging,
//! contact lists or custom properties, and no yearless dates, so those are
//! lost on export; `ADR`'s PO Box component round-trips only where a file
//! carries the legacy `… Address PO Box` columns. At most three e-mail
//! addresses survive a round trip - the same limit Outlook's format has.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::NaiveDate;
use std::collections::HashMap;
use std::error::Error as StdError;
use std::fmt;

use crate::vcard::{AddressField, Birthday, EmailField, Name, OtherProperty, TelephoneField, VCard};

/// Outlook's full classic column set, in the order Outlook writes them.
/// Import never depends on this order or on a column being present; export
/// always writes exactly these columns, empty cells included.
pub const OUTLOOK_CSV_COLUMNS: &[&str] = &[
    "First Name",
    "Middle Name",
    "Last Name",
    "Title",
    "Suffix",
    "Initials",
    "Web Page",
    "Gender",
    "Birthday",
    "Anniversary",
    "Location",
    "Language",
    "Internet Free Busy",
    "Notes",
    "Primary Phone",
    "Home Phone",
    "Home Phone 2",
    "Mobile Phone",
    "Pager",
    "Home Fax",
    "Home Street",
    "Home Street 2",
    "Home Street 3",
    "Home City",
    "Home State",
    "Home Postal Code",
    "Home Country/Region",
    "Home Address",
    "Business Phone",
    "Business Phone 2",
    "Business Fax",
    "Assistant's Phone",
    "Company Main Phone",
    "Business Street",
    "Business Street 2",
    "Business Street 3",
    "Business City",
    "Business State",
    "Business Postal Code",
    "Business Country/Region",
    "Business Address",
    "Other Phone",
    "Other Fax",
    "Other Street",
    "Other Street 2",
    "Other Street 3",
    "Other City",
    "Other State",
    "Other Postal Code",
    "Other Country/Region",
    "Other Address",
    "Callback Phone",
    "Car Phone",
    "ISDN",
    "Radio Phone",
    "TTY/TDD Phone",
    "Telex",
    "Assistant's Name",
    "Manager's Name",
    "Company",
    "Job Title",
    "Office Location",
    "Organizational ID Number",
    "Profession",
    "Account",
    "Business Home Page",
    "Spouse",
    "Children",
    "E-mail Address",
    "E-mail Type",
    "E-mail Display Name",
    "E-mail 2 Address",
    "E-mail 2 Type",
    "E-mail 2 Display Name",
    "E-mail 3 Address",
    "E-mail 3 Type",
    "E-mail 3 Display Name",
    "User 1",
    "User 1 Value",
    "User 2",
    "User 2 Value",
    "User 3",
    "User 3 Value",
    "User 4",
    "User 4 Value",
    "Categories",
];

/// Column spellings older Outlook versions wrote, kept matchable so files
/// from those exports still import their values.
const LEGACY_COLUMNS: &[&str] = &[
    "Home Country",
    "Business Country",
    "Other Country",
    "Home Address PO Box",
    "Business Address PO Box",
    "Other Address PO Box",
];

/// The e-mail columns, position 1-3, in Outlook's order.
const EMAIL_COLUMNS: [&str; 3] = ["E-mail Address", "E-mail 2 Address", "E-mail 3 Address"];
/// The matching `E-mail Type` columns (Outlook writes `SMTP`).
const EMAIL_TYPE_COLUMNS: [&str; 3] = ["E-mail Type", "E-mail 2 Type", "E-mail 3 Type"];

/// Import map: `(column, TYPE written on the phone)`. Ordered like the
/// canonical header so parsed cards keep a deterministic phone order.
const PHONE_IMPORT_COLUMNS: &[(&str, &[&str])] = &[
    ("Primary Phone", &["voice"]),
    ("Home Phone", &["home"]),
    ("Home Phone 2", &["home"]),
    ("Mobile Phone", &["cell"]),
    ("Pager", &["pager"]),
    ("Home Fax", &["home", "fax"]),
    ("Business Phone", &["work"]),
    ("Business Phone 2", &["work"]),
    ("Business Fax", &["work", "fax"]),
    ("Assistant's Phone", &["work"]),
    ("Company Main Phone", &["work"]),
    ("Other Phone", &["other"]),
    ("Other Fax", &["other", "fax"]),
    ("Callback Phone", &["callback"]),
    ("Car Phone", &["car"]),
    ("ISDN", &["isdn"]),
    ("Radio Phone", &["radio"]),
    ("TTY/TDD Phone", &["textphone"]),
    ("Telex", &["telex"]),
];

/// Export map: `(column, types a phone must carry)`, most specific first (a
/// work fax must not be claimed by "Business Phone"). Each phone is written
/// once; unmatched ones fill the first free column.
const PHONE_EXPORT_COLUMNS: &[(&str, &[&str])] = &[
    ("Primary Phone", &["voice"]),
    ("Mobile Phone", &["cell"]),
    ("Business Fax", &["work", "fax"]),
    ("Business Phone", &["work"]),
    ("Business Phone 2", &["work"]),
    ("Assistant's Phone", &["work"]),
    ("Company Main Phone", &["work"]),
    ("Home Fax", &["home", "fax"]),
    ("Home Phone", &["home"]),
    ("Home Phone 2", &["home"]),
    ("Other Fax", &["other", "fax"]),
    ("Other Phone", &["other"]),
    ("Pager", &["pager"]),
    ("Callback Phone", &["callback"]),
    ("Car Phone", &["car"]),
    ("ISDN", &["isdn"]),
    ("Radio Phone", &["radio"]),
    ("TTY/TDD Phone", &["textphone"]),
    ("Telex", &["telex"]),
];

/// Why an Outlook CSV text could not be read as a contact file at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContactsCsvError {
    /// The text is empty, has no header row, or its header names no column
    /// this module recognises - so it is not a contacts CSV in any Outlook
    /// spelling.
    NoHeader,
}

impl fmt::Display for ContactsCsvError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ContactsCsvError::NoHeader => write!(f, "the file has no recognisable column header"),
        }
    }
}

impl StdError for ContactsCsvError {}

/// Parses an Outlook CSV contact file into one [`VCard`] per non-blank row.
///
/// The returned cards carry no `UID`; the import path assigns one before the
/// PUT (already the policy for UID-less `.vcf` cards). A header-only file
/// yields an empty list, and rows whose every cell is empty or whose data all
/// falls in unrecognised columns are skipped.
pub fn parse(text: &str) -> Result<Vec<VCard>, ContactsCsvError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut rows = parse_rows(text).into_iter();
    let header = loop {
        match rows.next() {
            None => return Err(ContactsCsvError::NoHeader),
            Some(row) if row.iter().all(|cell| cell.trim().is_empty()) => continue,
            Some(row) => break row,
        }
    };
    let Some(index) = RowIndex::build(&header) else {
        return Err(ContactsCsvError::NoHeader);
    };
    let mut cards = Vec::new();
    for row in rows {
        if row.iter().all(|cell| cell.trim().is_empty()) {
            continue;
        }
        let card = card_from_row(&row, &index);
        if !card_is_empty(&card) {
            cards.push(card);
        }
    }
    Ok(cards)
}

/// Serialises cards as one Outlook CSV file: the canonical header row, then
/// one row per card, CRLF-terminated and RFC 4180-quoted where needed. The
/// caller adds the UTF-8 BOM (Outlook and Excel expect one).
pub fn export(cards: &[VCard]) -> String {
    let mut out = String::new();
    let header = OUTLOOK_CSV_COLUMNS.iter().map(|column| (*column).to_string()).collect::<Vec<_>>();
    write_row(&mut out, &header);
    for card in cards {
        write_row(&mut out, &row_for(card));
    }
    out
}

/// RFC 4180 reader: quoted fields may hold commas, doubled quotes and
/// newlines; rows end at LF, CRLF or CR.
fn parse_rows(text: &str) -> Vec<Vec<String>> {
    let mut rows = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if quoted {
            match c {
                '"' if chars.peek() == Some(&'"') => {
                    chars.next();
                    field.push('"');
                }
                '"' => quoted = false,
                _ => field.push(c),
            }
            continue;
        }
        match c {
            '"' if field.is_empty() => quoted = true,
            ',' => row.push(std::mem::take(&mut field)),
            '\r' | '\n' => {
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                }
                row.push(std::mem::take(&mut field));
                rows.push(std::mem::take(&mut row));
            }
            _ => field.push(c),
        }
    }
    if !field.is_empty() || !row.is_empty() {
        row.push(field);
        rows.push(row);
    }
    rows
}

/// Case- and punctuation-insensitive header key: `"E-mail 2 Address"` and
/// `"e-mail2address"` both key `email2address`.
fn normalize_header(value: &str) -> String {
    let mut key = value.trim().to_ascii_lowercase();
    key.retain(|c| c.is_ascii_alphanumeric());
    key
}

/// The header row mapped to column positions. `None` means no column can be
/// recognised, so the file is not a contacts CSV.
struct RowIndex {
    columns: HashMap<String, usize>,
}

impl RowIndex {
    fn build(header: &[String]) -> Option<Self> {
        let mut columns = HashMap::new();
        for (position, name) in header.iter().enumerate() {
            let key = normalize_header(name);
            if !key.is_empty() {
                columns.entry(key).or_insert(position);
            }
        }
        let recognised = OUTLOOK_CSV_COLUMNS
            .iter()
            .chain(LEGACY_COLUMNS)
            .any(|column| columns.contains_key(&normalize_header(column)));
        recognised.then_some(Self { columns })
    }

    /// The trimmed cell of `column` on `row`, or `""` when the column is
    /// absent from the header or the row is short.
    fn get<'a>(&self, row: &'a [String], column: &str) -> &'a str {
        self.columns
            .get(&normalize_header(column))
            .and_then(|position| row.get(*position))
            .map(|cell| cell.trim())
            .unwrap_or("")
    }
}

fn card_from_row(row: &[String], index: &RowIndex) -> VCard {
    let given = index.get(row, "First Name").to_string();
    let additional = index.get(row, "Middle Name").to_string();
    let family = index.get(row, "Last Name").to_string();
    let prefix = index.get(row, "Title").to_string();
    let suffix = index.get(row, "Suffix").to_string();
    let has_name = !(given.is_empty() && additional.is_empty() && family.is_empty() && prefix.is_empty() && suffix.is_empty());
    let name = has_name.then(|| Name {
        family: family.clone(),
        given: given.clone(),
        additional: additional.clone(),
        prefix: prefix.clone(),
        suffix: suffix.clone(),
    });

    let company = index.get(row, "Company").to_string();
    let full_name = {
        // Outlook's display name: the personal parts in reading order, the
        // honourific prefix excluded (it stays on `Name::prefix`). A
        // company-only or nameless contact falls back to the company, then
        // the first address, so the card still has something to show.
        let parts = [given.as_str(), additional.as_str(), family.as_str(), suffix.as_str()];
        let joined = parts.iter().filter(|part| !part.is_empty()).copied().collect::<Vec<_>>().join(" ");
        if !joined.is_empty() {
            Some(joined)
        } else if !company.is_empty() {
            Some(company.clone())
        } else {
            let email = index.get(row, "E-mail Address");
            (!email.is_empty()).then(|| email.to_string())
        }
    };
    let organization = (!company.is_empty()).then(|| vec![company]);

    let urls = {
        let mut urls = Vec::new();
        for column in ["Web Page", "Business Home Page"] {
            let url = index.get(row, column);
            if !url.is_empty() && !urls.iter().any(|seen| seen == url) {
                urls.push(url.to_string());
            }
        }
        urls
    };

    let birthday = parse_csv_date(index.get(row, "Birthday")).map(|date| Birthday { date, omit_year: false });
    let anniversary = parse_csv_date(index.get(row, "Anniversary")).map(|date| Birthday { date, omit_year: false });

    let emails = EMAIL_COLUMNS
        .iter()
        .zip(["work", "home", "other"])
        .filter_map(|(column, alias)| {
            let address = index.get(row, column);
            (!address.is_empty()).then(|| EmailField {
                types: vec![alias.to_string()],
                address: address.to_string(),
            })
        })
        .collect();

    let telephones = PHONE_IMPORT_COLUMNS
        .iter()
        .filter_map(|(column, types)| {
            let number = index.get(row, column);
            (!number.is_empty()).then(|| TelephoneField {
                types: types.iter().map(|alias| alias.to_string()).collect(),
                number: number.to_string(),
            })
        })
        .collect();

    let addresses = [("Business", "work"), ("Home", "home"), ("Other", "other")]
        .into_iter()
        .filter_map(|(prefix, alias)| address_from_row(row, index, prefix, alias))
        .collect();

    let other = {
        let spouse = index.get(row, "Spouse");
        if spouse.is_empty() {
            Vec::new()
        } else {
            vec![OtherProperty {
                name: "X-SPOUSE".to_string(),
                params: Vec::new(),
                value: spouse.to_string(),
            }]
        }
    };

    VCard {
        version: "4.0".to_string(),
        kind: None,
        uid: None,
        full_name,
        name,
        organization,
        title: non_empty(index.get(row, "Job Title")),
        file_as: None,
        photo: None,
        emails,
        telephones,
        addresses,
        urls,
        impp: Vec::new(),
        note: non_empty(index.get(row, "Notes")),
        birthday,
        anniversary,
        categories: index
            .get(row, "Categories")
            .split(',')
            .map(str::trim)
            .filter(|category| !category.is_empty())
            .map(str::to_string)
            .collect(),
        other,
    }
}

/// One `{prefix} …` postal-address block as a typed [`AddressField`], or
/// `None` when every component (and the legacy PO Box column) is empty. The
/// formatted `{prefix} Address` column is deliberately ignored: the
/// components carry the same information without guessing Outlook's
/// line-breaking.
fn address_from_row(row: &[String], index: &RowIndex, prefix: &str, alias: &str) -> Option<AddressField> {
    let component = |name: &str| index.get(row, &format!("{prefix} {name}"));
    let country = {
        let value = component("Country/Region");
        // Pre-2010 exports spell it without the "/Region" suffix.
        if value.is_empty() {
            component("Country")
        } else {
            value
        }
    };
    let street = ["Street", "Street 2", "Street 3"]
        .iter()
        .map(|name| component(name))
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("\n");
    let field = AddressField {
        types: vec![alias.to_string()],
        po_box: index.get(row, &format!("{prefix} Address PO Box")).to_string(),
        extended: String::new(),
        street,
        locality: component("City").to_string(),
        region: component("State").to_string(),
        postal_code: component("Postal Code").to_string(),
        country: country.to_string(),
        label: None,
    };
    let empty =
        field.po_box.is_empty() && field.street.is_empty() && field.locality.is_empty() && field.region.is_empty() && field.postal_code.is_empty() && field.country.is_empty();
    (!empty).then_some(field)
}

/// A card with no content at all (every cell of its row lived in columns
/// nobody maps) is not worth importing.
fn card_is_empty(card: &VCard) -> bool {
    card.full_name.is_none()
        && card.name.is_none()
        && card.organization.is_none()
        && card.title.is_none()
        && card.emails.is_empty()
        && card.telephones.is_empty()
        && card.addresses.is_empty()
        && card.urls.is_empty()
        && card.note.is_none()
        && card.birthday.is_none()
        && card.anniversary.is_none()
        && card.categories.is_empty()
        && card.other.is_empty()
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

/// Outlook CSV dates are formatted in the exporting machine's locale and
/// version, so accept the shapes seen in the wild: ISO `yyyy-mm-dd`,
/// slash/dot forms with a 2- or 4-digit year, and - because the US is the
/// common default - `m/d/y` with `d/m/y` only when the first number can't be
/// a month. `0/0/0`-style placeholders and invalid dates are `None`.
fn parse_csv_date(value: &str) -> Option<NaiveDate> {
    let parts = value.split(['/', '-', '.']).map(str::trim).collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let numbers = parts.iter().map(|part| part.parse::<u32>().ok()).collect::<Option<Vec<_>>>()?;
    let (year, month, day) = if parts[0].len() == 4 {
        (numbers[0], numbers[1], numbers[2])
    } else if parts[2].len() == 4 {
        date_order(numbers[0], numbers[1], numbers[2])
    } else if parts[2].len() <= 2 {
        let year = if numbers[2] < 50 { 2000 + numbers[2] } else { 1900 + numbers[2] };
        date_order(numbers[0], numbers[1], year)
    } else {
        return None;
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || year < 1 {
        return None;
    }
    NaiveDate::from_ymd_opt(year as i32, month, day)
}

/// Resolves `m/d` vs `d/m` for the two leading date numbers, defaulting to
/// Outlook's US reading and switching only when the first can't be a month.
fn date_order(first: u32, second: u32, year: u32) -> (u32, u32, u32) {
    if first > 12 && second <= 12 {
        (year, second, first)
    } else {
        (year, first, second)
    }
}

/// The full canonical row for one card.
fn row_for(card: &VCard) -> Vec<String> {
    let mut fields: HashMap<String, String> = HashMap::new();

    // Name columns: the structured name when the card has one, else the
    // display name is split so first/last survive the round trip (Outlook
    // does the same for contacts it only knows by display name).
    let fallback = split_full_name(card.full_name.as_deref());
    let (given, additional, family, prefix, suffix) = match &card.name {
        Some(name) => (
            name.given.as_str(),
            name.additional.as_str(),
            name.family.as_str(),
            name.prefix.as_str(),
            name.suffix.as_str(),
        ),
        None => (fallback.0.as_str(), "", fallback.1.as_str(), "", ""),
    };
    set(&mut fields, "First Name", given);
    set(&mut fields, "Middle Name", additional);
    set(&mut fields, "Last Name", family);
    set(&mut fields, "Title", prefix);
    set(&mut fields, "Suffix", suffix);

    if let Some(organization) = card.organization.as_deref() {
        let company = organization
            .iter()
            .map(String::as_str)
            .filter(|part| !part.trim().is_empty())
            .collect::<Vec<_>>()
            .join(", ");
        set(&mut fields, "Company", &company);
    }
    if let Some(title) = card.title.as_deref() {
        set(&mut fields, "Job Title", title);
    }
    if let Some(note) = card.note.as_deref() {
        set(&mut fields, "Notes", note);
    }
    let urls = card.urls.iter().map(|url| url.trim()).filter(|url| !url.is_empty()).collect::<Vec<_>>();
    if let Some(url) = urls.first() {
        set(&mut fields, "Web Page", url);
    }
    if let Some(url) = urls.get(1) {
        set(&mut fields, "Business Home Page", url);
    }
    if let Some(birthday) = card.birthday {
        set(&mut fields, "Birthday", &birthday.date.format("%Y-%m-%d").to_string());
    }
    if let Some(anniversary) = card.anniversary {
        set(&mut fields, "Anniversary", &anniversary.date.format("%Y-%m-%d").to_string());
    }
    set(&mut fields, "Categories", &card.categories.join(", "));

    // E-mail slots: claim by type (work → 1, home → 2, other → 3), then let
    // untyped addresses fill what is left; Outlook's format has three slots.
    let mut email_slots: [Option<&str>; 3] = [None, None, None];
    let mut claimed = vec![false; card.emails.len()];
    for (slot, alias) in ["work", "home", "other"].into_iter().enumerate() {
        for (position, email) in card.emails.iter().enumerate() {
            if !claimed[position] && !email.address.trim().is_empty() && email.types.iter().any(|value| value.eq_ignore_ascii_case(alias)) {
                claimed[position] = true;
                email_slots[slot] = Some(email.address.as_str());
                break;
            }
        }
    }
    for (position, email) in card.emails.iter().enumerate() {
        if claimed[position] || email.address.trim().is_empty() {
            continue;
        }
        if let Some(slot) = email_slots.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(email.address.as_str());
        }
    }
    for (slot, address) in email_slots.iter().enumerate() {
        if let Some(address) = address {
            set(&mut fields, EMAIL_COLUMNS[slot], address);
            set(&mut fields, EMAIL_TYPE_COLUMNS[slot], "SMTP");
        }
    }

    // Phones: the type tables above decide the column; leftovers fill the
    // first free column so no number is dropped.
    let mut claimed = vec![false; card.telephones.len()];
    let mut phones: Vec<(&'static str, String)> = Vec::new();
    for &(column, required) in PHONE_EXPORT_COLUMNS {
        for (position, phone) in card.telephones.iter().enumerate() {
            if claimed[position] || phone.number.trim().is_empty() {
                continue;
            }
            if required.iter().all(|wanted| phone.types.iter().any(|alias| alias.eq_ignore_ascii_case(wanted))) {
                claimed[position] = true;
                phones.push((column, phone.number.clone()));
                break;
            }
        }
    }
    for (position, phone) in card.telephones.iter().enumerate() {
        if claimed[position] || phone.number.trim().is_empty() {
            continue;
        }
        let free = PHONE_EXPORT_COLUMNS.iter().copied().find(|(column, _)| !phones.iter().any(|(filled, _)| filled == column));
        if let Some((column, _)) = free {
            phones.push((column, phone.number.clone()));
        }
    }
    for (column, number) in phones {
        set(&mut fields, column, &number);
    }

    // Addresses: claim by type (work → Business, home → Home, other → Other),
    // leftovers fill the first free block. The formatted `… Address` column
    // is written from the components, as Outlook does.
    let mut address_slots: [Option<&AddressField>; 3] = [None, None, None];
    let mut claimed = vec![false; card.addresses.len()];
    for (slot, alias) in ["work", "home", "other"].into_iter().enumerate() {
        for (position, address) in card.addresses.iter().enumerate() {
            if !claimed[position] && address.types.iter().any(|value| value.eq_ignore_ascii_case(alias)) {
                claimed[position] = true;
                address_slots[slot] = Some(address);
                break;
            }
        }
    }
    for (position, address) in card.addresses.iter().enumerate() {
        if claimed[position] {
            continue;
        }
        if let Some(slot) = address_slots.iter_mut().find(|slot| slot.is_none()) {
            *slot = Some(address);
        }
    }
    for (slot, address) in address_slots.iter().enumerate() {
        let Some(address) = address else { continue };
        let prefix = ["Business", "Home", "Other"][slot];
        let lines = address.street.split('\n').map(str::trim).filter(|line| !line.is_empty()).collect::<Vec<_>>();
        let mut street_lines = lines.iter();
        if let Some(line) = street_lines.next() {
            set(&mut fields, &format!("{prefix} Street"), line);
        }
        if let Some(line) = street_lines.next() {
            set(&mut fields, &format!("{prefix} Street 2"), line);
        }
        set(&mut fields, &format!("{prefix} Street 3"), &street_lines.copied().collect::<Vec<_>>().join("\n"));
        set(&mut fields, &format!("{prefix} City"), address.locality.trim());
        set(&mut fields, &format!("{prefix} State"), address.region.trim());
        set(&mut fields, &format!("{prefix} Postal Code"), address.postal_code.trim());
        set(&mut fields, &format!("{prefix} Country/Region"), address.country.trim());
        let mut formatted: Vec<&str> = Vec::new();
        if !address.po_box.trim().is_empty() {
            formatted.push(address.po_box.trim());
        }
        formatted.extend(lines.iter().copied());
        for part in [&address.locality, &address.region, &address.postal_code, &address.country] {
            if !part.trim().is_empty() {
                formatted.push(part.trim());
            }
        }
        set(&mut fields, &format!("{prefix} Address"), &formatted.join("\n"));
    }

    if let Some(spouse) = card.other.iter().find(|property| property.name.eq_ignore_ascii_case("X-SPOUSE")) {
        set(&mut fields, "Spouse", &spouse.value);
    }

    OUTLOOK_CSV_COLUMNS.iter().map(|column| fields.remove(*column).unwrap_or_default()).collect()
}

/// Splits a display name into `(given, family)` at the first whitespace; the
/// join of the two is the original display name again.
fn split_full_name(full_name: Option<&str>) -> (String, String) {
    let Some(full_name) = full_name.map(str::trim).filter(|name| !name.is_empty()) else {
        return (String::new(), String::new());
    };
    match full_name.split_once(char::is_whitespace) {
        Some((given, rest)) => (given.to_string(), rest.trim().to_string()),
        None => (full_name.to_string(), String::new()),
    }
}

fn set(fields: &mut HashMap<String, String>, column: &str, value: &str) {
    if !value.is_empty() {
        fields.insert(column.to_string(), value.to_string());
    }
}

fn write_row(out: &mut String, values: &[String]) {
    for (position, value) in values.iter().enumerate() {
        if position > 0 {
            out.push(',');
        }
        write_field(out, value);
    }
    out.push_str("\r\n");
}

fn write_field(out: &mut String, value: &str) {
    if value.contains([',', '"', '\n', '\r']) {
        out.push('"');
        for c in value.chars() {
            if c == '"' {
                out.push('"');
            }
            out.push(c);
        }
        out.push('"');
    } else {
        out.push_str(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = include_str!("../../../../test-fixtures/contacts/outlook-contacts.csv");

    fn empty_card() -> VCard {
        VCard {
            version: "4.0".to_string(),
            kind: None,
            uid: None,
            full_name: None,
            name: None,
            organization: None,
            title: None,
            file_as: None,
            photo: None,
            emails: Vec::new(),
            telephones: Vec::new(),
            addresses: Vec::new(),
            urls: Vec::new(),
            impp: Vec::new(),
            note: None,
            birthday: None,
            anniversary: None,
            categories: Vec::new(),
            other: Vec::new(),
        }
    }

    #[test]
    fn fixture_header_matches_the_canonical_columns() {
        assert!(FIXTURE.starts_with('\u{feff}'), "the fixture carries the UTF-8 BOM Outlook writes");
        let first = FIXTURE.lines().next().expect("fixture has a header");
        let first = first.strip_prefix('\u{feff}').unwrap_or(first);
        assert_eq!(first, OUTLOOK_CSV_COLUMNS.join(","));
    }

    #[test]
    fn parses_the_outlook_csv_fixture() {
        let cards = parse(FIXTURE).expect("fixture parses");
        assert_eq!(cards.len(), 3);

        let ada = &cards[0];
        assert_eq!(ada.full_name.as_deref(), Some("Ada Lovelace"));
        let name = ada.name.as_ref().expect("name");
        assert_eq!((name.prefix.as_str(), name.given.as_str(), name.family.as_str()), ("Ms", "Ada", "Lovelace"));
        assert_eq!(ada.organization.as_deref(), Some(&["Analytical Engines Ltd".to_string()][..]));
        assert_eq!(ada.title.as_deref(), Some("Mathematician"));
        assert_eq!(ada.urls, vec!["https://ada.example.com", "https://engines.example.com"]);
        let birthday = ada.birthday.expect("birthday");
        assert_eq!(birthday.date.to_string(), "1815-12-10");
        assert!(!birthday.omit_year);
        assert_eq!(ada.anniversary.map(|anniversary| anniversary.date.to_string()), Some("1835-07-08".to_string()));
        assert_eq!(ada.note.as_deref(), Some("Mathematician, first programmer.\nWrote the Notes."));
        assert_eq!(ada.categories, vec!["Colleagues", "History"]);
        assert_eq!(ada.emails.len(), 2);
        assert_eq!(ada.emails[0].types, vec!["work"]);
        assert_eq!(ada.emails[0].address, "ada@example.com");
        assert_eq!(ada.emails[1].types, vec!["home"]);
        assert_eq!(ada.emails[1].address, "ada.lovelace@example.net");
        assert!(ada.telephones.iter().any(|phone| phone.types == vec!["cell".to_string()] && phone.number == "+15550101"));
        assert!(ada
            .telephones
            .iter()
            .any(|phone| phone.types == vec!["work".to_string(), "fax".to_string()] && phone.number == "+442071234569"));
        assert_eq!(ada.addresses.len(), 2);
        let work = ada.addresses.iter().find(|address| address.types == vec!["work".to_string()]).expect("work address");
        assert_eq!(work.street, "1 Engine Way");
        assert_eq!(work.locality, "London");
        assert_eq!(work.postal_code, "SW1A 1AA");
        assert_eq!(work.country, "United Kingdom");
        assert!(ada.other.iter().any(|property| property.name == "X-SPOUSE" && property.value == "William King"));

        assert_eq!(cards[1].full_name.as_deref(), Some("Grace Hopper"));
        assert_eq!(cards[1].emails.len(), 1);

        let company_card = &cards[2];
        assert_eq!(company_card.full_name.as_deref(), Some("Acme Helpdesk"));
        assert_eq!(company_card.organization.as_deref(), Some(&["Acme Helpdesk".to_string()][..]));
        assert_eq!(company_card.note.as_deref(), Some("Hardware, support"));
    }

    #[test]
    fn export_reimports_the_fixture_fields() {
        let cards = parse(FIXTURE).expect("fixture parses");
        let exported = export(&cards);
        let reparsed = parse(&exported).expect("export parses");
        assert_eq!(reparsed.len(), cards.len());
        for (before, after) in cards.iter().zip(&reparsed) {
            assert_eq!(before.full_name, after.full_name);
            assert_eq!(before.organization, after.organization);
            assert_eq!(before.title, after.title);
            assert_eq!(before.urls, after.urls);
            assert_eq!(before.note, after.note);
            assert_eq!(before.birthday, after.birthday);
            assert_eq!(before.anniversary, after.anniversary);
            assert_eq!(before.categories, after.categories);
            assert_eq!(before.emails, after.emails);
            assert_eq!(before.telephones, after.telephones);
            assert_eq!(before.addresses, after.addresses);
            assert_eq!(before.other, after.other);
        }
    }

    #[test]
    fn header_matching_tolerates_case_spacing_punctuation_and_order() {
        let text = "\u{feff}Last Name , E-MAIL 2 ADDRESS,E-mail   Address, first_name,Company\r\nHopper,grace@example.net,grace@example.com,Grace,Navy\r\n";
        let cards = parse(text).expect("parses");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].full_name.as_deref(), Some("Grace Hopper"));
        assert_eq!(cards[0].organization.as_deref(), Some(&["Navy".to_string()][..]));
        assert_eq!(cards[0].emails.len(), 2);
        assert_eq!(cards[0].emails[0].types, vec!["work"]);
        assert_eq!(cards[0].emails[0].address, "grace@example.com");
        assert_eq!(cards[0].emails[1].types, vec!["home"]);
        assert_eq!(cards[0].emails[1].address, "grace@example.net");
    }

    #[test]
    fn unknown_columns_are_ignored_and_missing_ones_stay_unset() {
        let text = "Bananarama,First Name,E-mail Address\r\nzzz,Grace,grace@example.com\r\n";
        let cards = parse(text).expect("parses");
        assert_eq!(cards.len(), 1);
        assert_eq!(cards[0].full_name.as_deref(), Some("Grace"));
        assert_eq!(cards[0].emails[0].address, "grace@example.com");
        assert!(cards[0].organization.is_none());
        assert!(cards[0].telephones.is_empty());
    }

    #[test]
    fn empty_or_unrecognisable_input_is_an_error() {
        assert_eq!(parse("").unwrap_err(), ContactsCsvError::NoHeader);
        assert_eq!(parse(" \r\n,,,\r\n").unwrap_err(), ContactsCsvError::NoHeader);
        assert_eq!(parse("Name,Phone\r\nGrace,123\r\n").unwrap_err(), ContactsCsvError::NoHeader);
        assert!(parse("First Name,Last Name\r\n").unwrap().is_empty());
    }

    #[test]
    fn blank_and_unknown_only_rows_are_skipped() {
        let text = "First Name,Last Name,X-Custom\r\n\r\nGrace,Hopper,\r\n,,\r\n,,only-unknown\r\nAda,Lovelace,\r\n";
        let cards = parse(text).expect("parses");
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[1].full_name.as_deref(), Some("Ada Lovelace"));
    }

    #[test]
    fn quoted_values_with_commas_quotes_and_newlines_round_trip() {
        let mut card = empty_card();
        card.full_name = Some("Ada Lovelace".to_string());
        card.note = Some("He said \"hi\", twice.\nSecond line".to_string());
        let exported = export(&[card]);
        let parsed = parse(&exported).expect("export parses");
        assert_eq!(parsed[0].full_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(parsed[0].note.as_deref(), Some("He said \"hi\", twice.\nSecond line"));
    }

    #[test]
    fn dates_parse_outlooks_common_formats() {
        let iso = |date: &str| parse_csv_date(date).map(|date| date.to_string());
        assert_eq!(iso("1815-12-10").as_deref(), Some("1815-12-10"));
        assert_eq!(iso("1815/12/10").as_deref(), Some("1815-12-10"));
        assert_eq!(iso("12/10/1815").as_deref(), Some("1815-12-10"));
        assert_eq!(iso("25/12/1815").as_deref(), Some("1815-12-25"));
        assert_eq!(iso("1/2/85").as_deref(), Some("1985-01-02"));
        assert_eq!(iso("1/2/1985").as_deref(), Some("1985-01-02"));
        assert_eq!(iso("0/0/0"), None);
        assert_eq!(iso("0/0/0000"), None);
        assert_eq!(iso("31/02/2020"), None);
        assert_eq!(iso(""), None);
        assert_eq!(iso("soon"), None);
    }

    #[test]
    fn legacy_country_and_po_box_columns_are_matched() {
        let text = "First Name,Last Name,Home Country,Home Address PO Box,Home Street\r\nAda,Lovelace,United Kingdom,Box 12,1 Main St\r\n";
        let cards = parse(text).expect("parses");
        let home = &cards[0].addresses[0];
        assert_eq!(home.country, "United Kingdom");
        assert_eq!(home.po_box, "Box 12");
        assert_eq!(home.street, "1 Main St");
    }

    #[test]
    fn export_writes_the_canonical_header_and_caps_emails_at_three() {
        let mut card = empty_card();
        card.full_name = Some("Overloaded Contact".to_string());
        card.emails = (1..=4)
            .map(|n| EmailField {
                types: Vec::new(),
                address: format!("user{n}@example.com"),
            })
            .collect();
        let exported = export(&[card]);
        assert_eq!(exported.lines().next().expect("header"), OUTLOOK_CSV_COLUMNS.join(","));
        let parsed = parse(&exported).expect("export parses");
        assert_eq!(parsed[0].emails.len(), 3);
    }

    #[test]
    fn full_name_without_a_structured_name_survives_the_round_trip() {
        let mut card = empty_card();
        card.full_name = Some("Acme Helpdesk".to_string());
        card.organization = Some(vec!["Acme Helpdesk".to_string()]);
        let exported = export(&[card]);
        let parsed = parse(&exported).expect("export parses");
        assert_eq!(parsed[0].full_name.as_deref(), Some("Acme Helpdesk"));
    }
}
