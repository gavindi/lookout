/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use std::error::Error as StdError;
use std::fmt;

use crate::email::EmailAddress;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VCard {
    pub version: String,
    pub kind: Option<String>,
    pub uid: Option<String>,
    pub full_name: Option<String>,
    pub name: Option<Name>,
    pub organization: Option<Vec<String>>,
    pub title: Option<String>,
    pub emails: Vec<EmailField>,
    pub telephones: Vec<TelephoneField>,
    pub addresses: Vec<AddressField>,
    pub urls: Vec<String>,
    pub note: Option<String>,
    pub birthday: Option<Birthday>,
    pub categories: Vec<String>,
    pub other: Vec<OtherProperty>,
}

/// A contact's `BDAY` value. `omit_year` is the important distinction the
/// calendar layer needs: a full-date birthday happens once and has a year to
/// compute an age from, a yearless one (`BDAY:--MMDD`, or Apple's
/// `X-APPLE-OMIT-YEAR`) recurs every year - which is how the synthesized
/// birthdays calendar treats it. When `omit_year` is set the year carried in
/// `date` is an arbitrary anchor (2000 for `--MMDD` forms), not the birth
/// year.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Birthday {
    pub date: NaiveDate,
    #[serde(default)]
    pub omit_year: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Name {
    pub family: String,
    pub given: String,
    pub additional: String,
    pub prefix: String,
    pub suffix: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EmailField {
    pub types: Vec<String>,
    pub address: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TelephoneField {
    pub types: Vec<String>,
    pub number: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddressField {
    pub types: Vec<String>,
    pub po_box: String,
    pub extended: String,
    pub street: String,
    pub locality: String,
    pub region: String,
    pub postal_code: String,
    pub country: String,
    pub label: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OtherProperty {
    pub name: String,
    pub params: Vec<Parameter>,
    pub value: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parameter {
    pub name: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VCardError {
    MissingBegin,
    MissingEnd,
    InvalidLine(String),
    UnsupportedVersion(String),
    InvalidDate(String),
}

impl fmt::Display for VCardError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            VCardError::MissingBegin => write!(f, "missing BEGIN:VCARD"),
            VCardError::MissingEnd => write!(f, "missing END:VCARD"),
            VCardError::InvalidLine(line) => write!(f, "invalid vCard line: {line}"),
            VCardError::UnsupportedVersion(version) => write!(f, "unsupported vCard version: {version}"),
            VCardError::InvalidDate(value) => write!(f, "invalid date value: {value}"),
        }
    }
}

impl StdError for VCardError {}

impl VCard {
    pub fn parse(source: &str) -> Result<Self, VCardError> {
        let unfolded = unfold_lines(source);
        if !unfolded.iter().any(|line| line.eq_ignore_ascii_case("BEGIN:VCARD")) {
            return Err(VCardError::MissingBegin);
        }
        if !unfolded.iter().any(|line| line.eq_ignore_ascii_case("END:VCARD")) {
            return Err(VCardError::MissingEnd);
        }

        let mut card = VCard {
            version: "4.0".to_string(),
            kind: None,
            uid: None,
            full_name: None,
            name: None,
            organization: None,
            title: None,
            emails: Vec::new(),
            telephones: Vec::new(),
            addresses: Vec::new(),
            urls: Vec::new(),
            note: None,
            birthday: None,
            categories: Vec::new(),
            other: Vec::new(),
        };
        // Apple writes `X-APPLE-OMIT-YEAR:<year>` next to a `BDAY` whose year
        // is present only so clients can compute ages - the birthday itself
        // recurs yearly. Collected here because the property may precede or
        // follow the `BDAY` line, and applied once both have been read.
        let mut apple_omit_year = false;

        for line in unfolded {
            if line.eq_ignore_ascii_case("BEGIN:VCARD") || line.eq_ignore_ascii_case("END:VCARD") {
                continue;
            }
            let (name, params, value) = parse_property(&line)?;
            match name.to_ascii_uppercase().as_str() {
                "VERSION" => {
                    // vCard 3.0 (RFC 2426) is still what plenty of real-world
                    // CardDAV servers actually export - Google's among them -
                    // despite this app otherwise targeting 4.0 (RFC 6350).
                    // The two versions' syntax is close enough for every
                    // property parsed below that there's no need to branch
                    // on which one a given card uses.
                    if value != "4.0" && value != "3.0" {
                        return Err(VCardError::UnsupportedVersion(value));
                    }
                    card.version = value;
                }
                "KIND" => card.kind = Some(value),
                "UID" => card.uid = Some(value),
                "FN" => card.full_name = Some(value),
                "N" => card.name = Some(parse_name(&value)),
                "ORG" => card.organization = Some(value.split(';').map(|part| part.to_string()).collect()),
                "TITLE" => card.title = Some(value),
                "EMAIL" => card.emails.push(parse_email_field(&params, &value)),
                "TEL" => card.telephones.push(parse_telephone_field(&params, &value)),
                "ADR" => card.addresses.push(parse_address_field(&params, &value)?),
                "URL" => card.urls.push(value),
                "NOTE" => card.note = Some(value),
                "BDAY" => card.birthday = Some(parse_birthday(&value)?),
                "X-APPLE-OMIT-YEAR" => apple_omit_year = true,
                "CATEGORIES" => card.categories = value.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect(),
                _ => card.other.push(OtherProperty { name, params, value }),
            }
        }

        if apple_omit_year {
            if let Some(birthday) = card.birthday.as_mut() {
                birthday.omit_year = true;
            }
        }

        Ok(card)
    }

    /// Parses a document containing one or more vCards - the shape of the
    /// `.vcf` files an export or import moves around - into one card per
    /// `BEGIN:VCARD`/`END:VCARD` pair. Stray text between cards (blank
    /// lines, an export preamble, ...) is ignored. Each block is parsed by
    /// [`Self::parse`] individually, so one malformed card reports its own
    /// `Err` without aborting the rest of the file - the import flow can
    /// skip and count it the way `lookout-dav` skips unparseable cards from
    /// a multiget. A trailing `BEGIN:VCARD` with no matching `END` surfaces
    /// as an `Err` too, rather than being silently dropped.
    pub fn parse_all(source: &str) -> Vec<Result<VCard, VCardError>> {
        let mut cards = Vec::new();
        let mut current: Option<String> = None;
        for line in source.replace("\r\n", "\n").lines() {
            if line.eq_ignore_ascii_case("BEGIN:VCARD") {
                current = Some(line.to_string());
            } else if let Some(block) = current.as_mut() {
                block.push('\n');
                block.push_str(line);
                if line.eq_ignore_ascii_case("END:VCARD") {
                    let block = current.take().expect("set just above");
                    cards.push(Self::parse(&block));
                }
            }
        }
        if let Some(block) = current {
            cards.push(Self::parse(&block));
        }
        cards
    }

    /// Renders this vCard as RFC 6350 text - the writer half of the parser
    /// above. (Named `serialize` rather than `to_string` so the inherent
    /// method doesn't shadow the `Display`-derived one clippy would otherwise
    /// flag as redundant.)
    pub fn serialize(&self) -> String {
        let mut lines = vec!["BEGIN:VCARD".to_string(), format!("VERSION:{}", self.version)];

        if let Some(kind) = &self.kind {
            lines.push(format!("KIND:{}", escape_value(kind)));
        }
        if let Some(uid) = &self.uid {
            lines.push(format!("UID:{}", escape_value(uid)));
        }
        if let Some(fn_value) = &self.full_name {
            lines.push(format!("FN:{}", escape_value(fn_value)));
        }
        if let Some(name) = &self.name {
            lines.push(format!(
                "N:{};{};{};{};{}",
                escape_value(&name.family),
                escape_value(&name.given),
                escape_value(&name.additional),
                escape_value(&name.prefix),
                escape_value(&name.suffix)
            ));
        }
        if let Some(org) = &self.organization {
            lines.push(format!("ORG:{}", org.iter().map(|part| escape_value(part)).collect::<Vec<_>>().join(";")));
        }
        if let Some(title) = &self.title {
            lines.push(format!("TITLE:{}", escape_value(title)));
        }
        for email in &self.emails {
            let params = param_string(&email.types, "TYPE");
            lines.push(format!("EMAIL{}:{}", params, escape_value(&email.address)));
        }
        for tel in &self.telephones {
            let params = param_string(&tel.types, "TYPE");
            lines.push(format!("TEL{}:{}", params, escape_value(&tel.number)));
        }
        for adr in &self.addresses {
            let mut param_parts = Vec::new();
            if !adr.types.is_empty() {
                param_parts.push(format!("TYPE={}", adr.types.join(",")));
            }
            if let Some(label) = &adr.label {
                param_parts.push(format!("LABEL={}", quote_param_value(label)));
            }
            let params = if param_parts.is_empty() { String::new() } else { format!(";{}", param_parts.join(";")) };
            lines.push(format!(
                "ADR{}:{};{};{};{};{};{};{}",
                params,
                escape_value(&adr.po_box),
                escape_value(&adr.extended),
                escape_value(&adr.street),
                escape_value(&adr.locality),
                escape_value(&adr.region),
                escape_value(&adr.postal_code),
                escape_value(&adr.country)
            ));
        }
        for url in &self.urls {
            lines.push(format!("URL:{}", escape_value(url)));
        }
        if let Some(note) = &self.note {
            lines.push(format!("NOTE:{}", escape_value(note)));
        }
        if let Some(bday) = &self.birthday {
            if bday.omit_year {
                // RFC 6350 §3.3.4's yearless form; the X- property is Apple's
                // marker that the year is decorative (their exports carry
                // both), so a round-trip through Apple-oriented servers
                // keeps the no-year semantics.
                lines.push(format!("BDAY:--{}", bday.date.format("%m%d")));
                lines.push(format!("X-APPLE-OMIT-YEAR:{}", bday.date.format("%Y")));
            } else {
                lines.push(format!("BDAY:{}", bday.date.format("%Y-%m-%d")));
            }
        }
        if !self.categories.is_empty() {
            lines.push(format!("CATEGORIES:{}", self.categories.join(",")));
        }
        for other in &self.other {
            let param_string = other
                .params
                .iter()
                .map(|param| format!("{}={}", param.name, param.values.join(",")))
                .collect::<Vec<_>>()
                .join(";");
            let params = if param_string.is_empty() { String::new() } else { format!(";{}", param_string) };
            lines.push(format!("{}{}:{}", other.name, params, escape_value(&other.value)));
        }

        lines.push("END:VCARD".to_string());
        lines.into_iter().map(fold_line).collect::<Vec<_>>().join("\r\n")
    }

    /// Returns every email address exposed by this vCard, using the contact's
    /// full name or structured name as the display label when available.
    pub fn email_addresses(&self) -> Vec<EmailAddress> {
        let display_name = self.full_name.clone().or_else(|| {
            self.name.as_ref().and_then(|name| {
                let mut parts = Vec::new();
                if !name.prefix.trim().is_empty() {
                    parts.push(name.prefix.clone());
                }
                if !name.given.trim().is_empty() {
                    parts.push(name.given.clone());
                }
                if !name.family.trim().is_empty() {
                    parts.push(name.family.clone());
                }
                if !name.suffix.trim().is_empty() {
                    parts.push(name.suffix.clone());
                }
                let combined = parts.join(" ");
                if combined.trim().is_empty() {
                    None
                } else {
                    Some(combined)
                }
            })
        });

        self.emails
            .iter()
            .map(|email| EmailAddress {
                name: display_name.clone(),
                address: email.address.clone(),
            })
            .collect()
    }
}

fn unfold_lines(source: &str) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    for raw in source.replace("\r\n", "\n").lines() {
        if raw.starts_with(' ') || raw.starts_with('\t') {
            if let Some(last) = lines.last_mut() {
                let mut continuation = raw.to_string();
                continuation.remove(0);
                last.push_str(&continuation);
            }
        } else {
            lines.push(raw.to_string());
        }
    }
    lines
}

fn parse_property(line: &str) -> Result<(String, Vec<Parameter>, String), VCardError> {
    let (key, value) = line.split_once(':').ok_or_else(|| VCardError::InvalidLine(line.to_string()))?;
    let (name, params) = if let Some((name, param_str)) = key.split_once(';') {
        (name.to_string(), parse_params(param_str))
    } else {
        (key.to_string(), Vec::new())
    };
    Ok((name, params, unescape_value(value)))
}

fn parse_params(raw: &str) -> Vec<Parameter> {
    let mut params = Vec::new();
    for part in raw.split(';') {
        if part.is_empty() {
            continue;
        }
        if let Some((name, value)) = part.split_once('=') {
            let values = split_param_values(value);
            params.push(Parameter { name: name.to_string(), values });
        } else {
            params.push(Parameter {
                name: part.to_string(),
                values: vec![],
            });
        }
    }
    params
}

fn split_param_values(value: &str) -> Vec<String> {
    let value = value.trim();
    let unquoted = if value.starts_with('"') && value.ends_with('"') && value.len() >= 2 {
        &value[1..value.len() - 1]
    } else {
        value
    };
    unquoted.split(',').map(|s| unescape_value(s.trim())).filter(|s| !s.is_empty()).collect()
}

fn parse_name(value: &str) -> Name {
    let parts: Vec<String> = value.split(';').map(str::to_string).collect();
    Name {
        family: parts.first().cloned().unwrap_or_default(),
        given: parts.get(1).cloned().unwrap_or_default(),
        additional: parts.get(2).cloned().unwrap_or_default(),
        prefix: parts.get(3).cloned().unwrap_or_default(),
        suffix: parts.get(4).cloned().unwrap_or_default(),
    }
}

fn parse_email_field(params: &[Parameter], value: &str) -> EmailField {
    EmailField {
        types: param_values(params, "TYPE"),
        address: value.to_string(),
    }
}

fn parse_telephone_field(params: &[Parameter], value: &str) -> TelephoneField {
    TelephoneField {
        types: param_values(params, "TYPE"),
        number: value.to_string(),
    }
}

fn parse_address_field(params: &[Parameter], value: &str) -> Result<AddressField, VCardError> {
    let parts: Vec<String> = value.split(';').map(unescape_value).collect();
    let label = param_values(params, "LABEL").first().cloned();
    Ok(AddressField {
        types: param_values(params, "TYPE"),
        po_box: parts.first().cloned().unwrap_or_default(),
        extended: parts.get(1).cloned().unwrap_or_default(),
        street: parts.get(2).cloned().unwrap_or_default(),
        locality: parts.get(3).cloned().unwrap_or_default(),
        region: parts.get(4).cloned().unwrap_or_default(),
        postal_code: parts.get(5).cloned().unwrap_or_default(),
        country: parts.get(6).cloned().unwrap_or_default(),
        label,
    })
}

fn parse_birthday(value: &str) -> Result<Birthday, VCardError> {
    let value = value.trim();
    // RFC 6350 §3.3.4: `BDAY:--MMDD` - a yearless birthday that recurs every
    // year. Google and Apple both export this shape for contacts whose birth
    // year is unknown (or private). The 2000 anchor is a leap year, so every
    // month/day combination round-trips through `NaiveDate`.
    if let Some(rest) = value.strip_prefix("--") {
        if rest.len() >= 4 {
            let month = rest[..2].parse::<u32>().ok();
            let day = rest[2..4].parse::<u32>().ok();
            if let (Some(month), Some(day)) = (month, day) {
                if let Some(date) = NaiveDate::from_ymd_opt(2000, month, day) {
                    return Ok(Birthday { date, omit_year: true });
                }
            }
        }
        return Err(VCardError::InvalidDate(value.to_string()));
    }
    // A full date or date-time - ISO-ish (`1990-05-23T12:00:00Z`) or basic
    // (`19900523T120000Z`). Only the date part matters: the time of day a
    // birthday carries is never rendered.
    let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
    if digits.len() >= 8 {
        let year = digits[..4].parse::<i32>().ok();
        let month = digits[4..6].parse::<u32>().ok();
        let day = digits[6..8].parse::<u32>().ok();
        if let (Some(year), Some(month), Some(day)) = (year, month, day) {
            if let Some(date) = NaiveDate::from_ymd_opt(year, month, day) {
                return Ok(Birthday { date, omit_year: false });
            }
        }
    }
    Err(VCardError::InvalidDate(value.to_string()))
}

fn param_values(params: &[Parameter], key: &str) -> Vec<String> {
    params
        .iter()
        .find(|param| param.name.eq_ignore_ascii_case(key))
        .map(|param| param.values.clone())
        .unwrap_or_default()
}

fn escape_value(value: &str) -> String {
    value.replace('\\', "\\\\").replace('\n', "\\n").replace(';', "\\;").replace(',', "\\,")
}

fn unescape_value(value: &str) -> String {
    let mut result = String::new();
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if let Some(next) = chars.next() {
                result.push(match next {
                    'n' | 'N' => '\n',
                    '\\' => '\\',
                    ';' => ';',
                    ',' => ',',
                    ':' => ':',
                    other => other,
                });
            }
        } else {
            result.push(ch);
        }
    }
    result
}

fn param_string(types: &[String], name: &str) -> String {
    if types.is_empty() {
        String::new()
    } else {
        format!(";{}={}", name, types.join(","))
    }
}

fn quote_param_value(value: &str) -> String {
    let escaped = escape_value(value);
    if escaped.contains(',') || escaped.contains(';') || escaped.contains(':') || escaped.contains(' ') || escaped.contains('\n') {
        format!("\"{}\"", escaped.replace('"', "\\\""))
    } else {
        escaped
    }
}

fn fold_line(line: String) -> String {
    let max = 75;
    if line.len() <= max {
        return line;
    }

    let mut result = String::new();
    let mut current = String::new();
    for ch in line.chars() {
        let next_len = current.len() + ch.len_utf8();
        if next_len > max {
            result.push_str(&current);
            result.push_str("\r\n ");
            current.clear();
        }
        current.push(ch);
    }
    result.push_str(&current);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_vcard() {
        let text = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Jane Doe\r\nN:Doe;Jane;;;\r\nEMAIL;TYPE=work:jane.doe@example.com\r\nTEL;TYPE=cell:+15551234567\r\nEND:VCARD\r\n";
        let card = VCard::parse(text).unwrap();
        assert_eq!(card.full_name.as_deref(), Some("Jane Doe"));
        assert_eq!(card.name.unwrap().family, "Doe");
        assert_eq!(card.emails.len(), 1);
        assert_eq!(card.emails[0].address, "jane.doe@example.com");
        assert_eq!(card.emails[0].types, vec!["work"]);
        assert_eq!(card.telephones[0].number, "+15551234567");
        assert_eq!(card.telephones[0].types, vec!["cell"]);
    }

    #[test]
    fn round_trips_vcard_to_string_and_parse() {
        let card = VCard {
            version: "4.0".to_string(),
            kind: Some("individual".to_string()),
            uid: Some("1234".to_string()),
            full_name: Some("Ada Lovelace".to_string()),
            name: Some(Name {
                family: "Lovelace".to_string(),
                given: "Ada".to_string(),
                additional: String::new(),
                prefix: String::new(),
                suffix: String::new(),
            }),
            organization: Some(vec!["Example Corp".to_string()]),
            title: Some("Engineer".to_string()),
            emails: vec![EmailField {
                types: vec!["work".to_string()],
                address: "ada@example.com".to_string(),
            }],
            telephones: vec![TelephoneField {
                types: vec!["home".to_string()],
                number: "+11234567890".to_string(),
            }],
            addresses: vec![AddressField {
                types: vec!["home".to_string()],
                po_box: String::new(),
                extended: String::new(),
                street: "123 Main St".to_string(),
                locality: "Anytown".to_string(),
                region: "CA".to_string(),
                postal_code: "94102".to_string(),
                country: "USA".to_string(),
                label: Some("123 Main St\nAnytown, CA 94102".to_string()),
            }],
            urls: vec!["https://example.com".to_string()],
            note: Some("Example contact".to_string()),
            birthday: Some(Birthday {
                date: NaiveDate::from_ymd_opt(1990, 5, 23).unwrap(),
                omit_year: false,
            }),
            categories: vec!["friend".to_string(), "colleague".to_string()],
            other: vec![],
        };
        let text = card.serialize();
        let parsed = VCard::parse(&text).unwrap();
        assert_eq!(parsed.full_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(parsed.organization.as_ref().unwrap()[0], "Example Corp");
        assert_eq!(parsed.categories, vec!["friend", "colleague"]);
        assert_eq!(
            parsed.birthday,
            Some(Birthday {
                date: NaiveDate::from_ymd_opt(1990, 5, 23).unwrap(),
                omit_year: false,
            })
        );
    }

    #[test]
    fn parses_folded_vcard_lines() {
        let text = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:John \r\n Doe\r\nN:Doe;John;;;\r\nEND:VCARD\r\n";
        let card = VCard::parse(text).unwrap();
        assert_eq!(card.full_name.as_deref(), Some("John Doe"));
    }

    #[test]
    fn parse_all_splits_multiple_cards_and_ignores_stray_text() {
        let text = "an export preamble\nexported-by: alice\n\nBEGIN:VCARD\r\nVERSION:4.0\r\nFN:Jane Doe\r\nUID:jane@example.com\r\nEND:VCARD\r\n\r\n\r\nBEGIN:VCARD\r\nVERSION:4.0\r\nFN:John Roe\r\nUID:john@example.com\r\nEND:VCARD\r\n";
        let cards: Vec<VCard> = VCard::parse_all(text).into_iter().collect::<Result<_, _>>().unwrap();
        assert_eq!(cards.len(), 2);
        assert_eq!(cards[0].full_name.as_deref(), Some("Jane Doe"));
        assert_eq!(cards[1].uid.as_deref(), Some("john@example.com"));
    }

    #[test]
    fn parse_all_reports_one_bad_card_without_dropping_the_rest() {
        let text = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Good\r\nEND:VCARD\r\nBEGIN:VCARD\r\nVERSION:9.9\r\nEND:VCARD\r\nBEGIN:VCARD\r\nVERSION:4.0\r\nFN:Also Good\r\nEND:VCARD\r\n";
        let results = VCard::parse_all(text);
        assert_eq!(results.len(), 3);
        assert!(results[0].is_ok());
        assert!(matches!(results[1], Err(VCardError::UnsupportedVersion(_))), "{:?}", results[1]);
        assert!(results[2].is_ok());
    }

    #[test]
    fn parse_all_treats_an_unterminated_trailing_card_as_an_error() {
        let text = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Fine\r\nEND:VCARD\r\nBEGIN:VCARD\r\nVERSION:4.0\r\nFN:Cut off\r\n";
        let results = VCard::parse_all(text);
        assert_eq!(results.len(), 2);
        assert!(results[0].is_ok());
        assert!(matches!(results[1], Err(VCardError::MissingEnd)), "{:?}", results[1]);
    }

    #[test]
    fn parse_all_of_an_empty_or_cardless_document_is_empty() {
        assert!(VCard::parse_all("").is_empty());
        assert!(VCard::parse_all("no cards here\njust text\n").is_empty());
    }

    #[test]
    fn parses_full_date_birthday_forms() {
        for (bday, expected) in [
            ("1990-05-23", NaiveDate::from_ymd_opt(1990, 5, 23).unwrap()),
            ("19900523", NaiveDate::from_ymd_opt(1990, 5, 23).unwrap()),
            ("19900523T120000Z", NaiveDate::from_ymd_opt(1990, 5, 23).unwrap()),
            ("1990-05-23T12:00:00Z", NaiveDate::from_ymd_opt(1990, 5, 23).unwrap()),
            ("1990-05-23T12:00:00", NaiveDate::from_ymd_opt(1990, 5, 23).unwrap()),
        ] {
            let text = format!("BEGIN:VCARD\r\nVERSION:4.0\r\nFN:A\r\nBDAY:{bday}\r\nEND:VCARD\r\n");
            let card = VCard::parse(&text).unwrap();
            assert_eq!(card.birthday, Some(Birthday { date: expected, omit_year: false }), "BDAY:{bday}");
        }
    }

    #[test]
    fn parses_yearless_birthday_and_marks_omit_year() {
        let text = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:A\r\nBDAY:--0523\r\nEND:VCARD\r\n";
        let card = VCard::parse(text).unwrap();
        assert_eq!(
            card.birthday,
            Some(Birthday {
                date: NaiveDate::from_ymd_opt(2000, 5, 23).unwrap(),
                omit_year: true,
            })
        );
    }

    #[test]
    fn parses_apple_omit_year_whichever_line_order() {
        let before = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:A\r\nX-APPLE-OMIT-YEAR:1979\r\nBDAY:1979-05-23\r\nEND:VCARD\r\n";
        let after = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:A\r\nBDAY:1979-05-23\r\nX-APPLE-OMIT-YEAR:1979\r\nEND:VCARD\r\n";
        for text in [before, after] {
            let card = VCard::parse(text).unwrap();
            assert!(card.birthday.unwrap().omit_year);
        }
    }

    #[test]
    fn round_trips_yearless_birthday_as_rfc_6350_form() {
        let text = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:A\r\nBDAY:--0523\r\nEND:VCARD\r\n";
        let card = VCard::parse(text).unwrap();
        let serialized = card.serialize();
        assert!(serialized.contains("BDAY:--0523"), "{serialized}");
        let reparsed = VCard::parse(&serialized).unwrap();
        assert!(reparsed.birthday.unwrap().omit_year);
    }

    #[test]
    fn parses_escaped_values() {
        let text = "BEGIN:VCARD\r\nVERSION:4.0\r\nFN:Foo\\, Bar\r\nNOTE:Line1\\nLine2\r\nEND:VCARD\r\n";
        let card = VCard::parse(text).unwrap();
        assert_eq!(card.full_name.as_deref(), Some("Foo, Bar"));
        assert_eq!(card.note.as_deref(), Some("Line1\nLine2"));
    }

    #[test]
    fn vcard_email_addresses_use_full_name_when_present() {
        let card = VCard {
            version: "4.0".to_string(),
            kind: None,
            uid: None,
            full_name: Some("Ada Lovelace".to_string()),
            name: Some(Name {
                family: "Lovelace".to_string(),
                given: "Ada".to_string(),
                additional: String::new(),
                prefix: String::new(),
                suffix: String::new(),
            }),
            organization: None,
            title: None,
            emails: vec![EmailField {
                types: vec!["work".to_string()],
                address: "ada@example.com".to_string(),
            }],
            telephones: Vec::new(),
            addresses: Vec::new(),
            urls: Vec::new(),
            note: None,
            birthday: None,
            categories: Vec::new(),
            other: Vec::new(),
        };
        let addresses = card.email_addresses();
        assert_eq!(
            addresses,
            vec![EmailAddress {
                name: Some("Ada Lovelace".to_string()),
                address: "ada@example.com".to_string()
            }]
        );
    }
}
