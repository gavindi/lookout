//! Microsoft Graph ⇄ `lookout-core` contact mapping (OUTLOOK_PARITY.md §5.2,
//! item 5.2 / A-04, P-11).
//!
//! Graph's `/me/contactFolders`, `/me/contacts` and `/me/people` speak a JSON
//! contact/person vocabulary, and the People screen speaks `lookout_core`'s
//! `VCard`. This module is the one place the two are translated, so neither
//! the sync loop nor the feature code hard-codes Graph property names
//! (§10 "Interop strings"). Like `calendar`, it is deliberately pure - no
//! HTTP, no `GraphClient` - so every rule is unit-testable against inline
//! JSON. The HTTP transport lives in [`client`] (`ContactsClient`).
//!
//! Deliberately *not* carried, and why:
//! - **Contact groups / lists** (`KIND:group`): Graph v1.0 exposes no contact
//!   group endpoint, so Microsoft 365 contact lists stay local-only.
//! - **Anniversary**: not a v1.0 contact property (Outlook stores it as an
//!   extended property), so it is neither read nor written.
//! - **Photo**: fetched separately (`/photo/$value`), not part of the contact
//!   JSON.
//!
//! Scalar fields with no vCard home (office location, manager, assistant,
//! profession, nickname, initials, spouse, children) round-trip as `other`
//! properties so an edit in Lookout can't silently clear them in Outlook.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
pub mod client;

pub use client::ContactsClient;

use chrono::NaiveDate;
use lookout_core::{AddressField, Birthday, EmailField, ImppField, Name, OtherProperty, TelephoneField, VCard};
use serde_json::{json, Value};

/// One contact folder (`/me/contactFolders`): the People screen's address-book
/// row. `id` is the Graph folder id - the write target for a create in that
/// folder. The app maps this to its own `AddressBookInfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContactFolder {
    pub id: String,
    pub display_name: String,
}

/// One contact as read from Graph: the folder-scoped `id`, the raw
/// `changeKey` (used as the write precondition, matching the calendar
/// backend's `etag` convention), and the parsed card.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphContact {
    pub id: String,
    pub etag: Option<String>,
    pub card: VCard,
}

/// One directory/GAL entry (`/me/people` or `/users?$search=`), reduced to
/// what the recipient autocomplete and profile card need. `id` is the Graph
/// user id, used for a directory photo (`/users/{id}/photo/$value`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirectoryPerson {
    pub id: Option<String>,
    pub name: Option<String>,
    pub address: String,
    pub title: Option<String>,
    pub company: Option<String>,
}

/// The scalar Graph contact fields that have no vCard home, mapped to the
/// `X-`-prefixed custom-property name under which they round-trip through
/// `VCard::other` (read into the card, written back out on save).
const OTHER_SCALARS: &[(&str, &str)] = &[
    ("officeLocation", "X-OFFICE"),
    ("manager", "X-MANAGER"),
    ("assistantName", "X-ASSISTANT"),
    ("profession", "X-PROFESSION"),
    ("nickName", "X-NICKNAME"),
    ("initials", "X-INITIALS"),
    ("spouseName", "X-SPOUSE"),
];

/// Maps a Graph `contactFolder` to a [`ContactFolder`], or `None` when it has
/// no id.
pub fn contact_folder_from_graph(value: &Value) -> Option<ContactFolder> {
    let id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    let display_name = value
        .get("displayName")
        .and_then(Value::as_str)
        .filter(|name| !name.trim().is_empty())
        .unwrap_or("Contacts")
        .to_string();
    Some(ContactFolder { id: id.to_string(), display_name })
}

/// The Graph id of a `/delta` tombstone (`{"id": "…", "@removed": {…}}`), or
/// `None` for a live record.
pub fn removed_id(value: &Value) -> Option<String> {
    value.get("@removed")?;
    value.get("id").and_then(Value::as_str).map(str::to_string)
}

/// Maps a Graph `contact` to a [`GraphContact`]. A `/delta` tombstone, or a
/// record with no id, is `None`.
pub fn contact_from_graph(value: &Value) -> Option<GraphContact> {
    if value.get("@removed").is_some() {
        return None;
    }
    let id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    Some(GraphContact {
        id: id.to_string(),
        etag: change_key(value),
        card: card_from_graph(value, id),
    })
}

/// The `changeKey` a write uses as its `If-Match` precondition.
pub fn contact_etag(value: &Value) -> Option<String> {
    change_key(value)
}

/// The JSON body to `POST`/`PATCH` a Graph contact from a [`VCard`]. Only the
/// fields the model carries are written; a field Lookout doesn't know about is
/// left absent (Graph merges a `PATCH`, so an absent field is untouched).
pub fn contact_to_graph(card: &VCard) -> Value {
    let mut body = json!({});

    if let Some(name) = &card.full_name {
        set(&mut body, "displayName", name);
    }
    if let Some(name) = &card.name {
        set(&mut body, "givenName", &name.given);
        set(&mut body, "middleName", &name.additional);
        set(&mut body, "surname", &name.family);
        set(&mut body, "generation", &name.suffix);
        // Outlook keeps the name prefix ("Mr.", "Dr.") in `title`; the job
        // title lives in `jobTitle` below.
        set(&mut body, "title", &name.prefix);
    }
    if let Some(title) = &card.title {
        set(&mut body, "jobTitle", title);
    }
    if let Some(file_as) = &card.file_as {
        set(&mut body, "fileAs", file_as);
    }
    if let Some(organization) = &card.organization {
        if let Some(company) = organization.first() {
            set(&mut body, "companyName", company);
        }
        if let Some(department) = organization.get(1) {
            set(&mut body, "department", department);
        }
    }

    let emails: Vec<Value> = card
        .emails
        .iter()
        .filter(|email| !email.address.trim().is_empty())
        .map(|email| json!({ "address": email.address.trim() }))
        .collect();
    body["emailAddresses"] = json!(emails);

    let mut business = Vec::new();
    let mut home = Vec::new();
    let mut mobile = None;
    for phone in &card.telephones {
        let number = phone.number.trim();
        if number.is_empty() {
            continue;
        }
        let kinds: Vec<String> = phone.types.iter().map(|t| t.to_ascii_lowercase()).collect();
        if kinds.iter().any(|kind| kind == "cell" || kind == "mobile") {
            mobile = Some(number.to_string());
        } else if kinds.iter().any(|kind| kind == "home") {
            home.push(number.to_string());
        } else {
            business.push(number.to_string());
        }
    }
    body["businessPhones"] = json!(business);
    body["homePhones"] = json!(home);
    if let Some(mobile) = mobile {
        body["mobilePhone"] = json!(mobile);
    }

    for address in &card.addresses {
        let kinds: Vec<String> = address.types.iter().map(|t| t.to_ascii_lowercase()).collect();
        let key = if kinds.iter().any(|kind| kind == "home") {
            "homeAddress"
        } else if kinds.iter().any(|kind| kind == "other") {
            "otherAddress"
        } else {
            "businessAddress"
        };
        if let Some(physical) = address_to_graph(address) {
            body[key] = physical;
        }
    }

    let im: Vec<String> = card.impp.iter().map(|impp| impp.uri.trim().to_string()).filter(|uri| !uri.is_empty()).collect();
    body["imAddresses"] = json!(im);

    if let Some(note) = &card.note {
        set(&mut body, "personalNotes", note);
    }
    if !card.categories.is_empty() {
        body["categories"] = json!(card.categories);
    }
    if let Some(birthday) = &card.birthday {
        // Graph's birthday is a full date; a yearless card keeps the model's
        // anchor year (2000) rather than writing a malformed value.
        body["birthday"] = json!(birthday.date.format("%Y-%m-%d").to_string());
    }
    if let Some(url) = card.urls.iter().find(|url| !url.trim().is_empty()) {
        body["businessHomePage"] = json!(url.trim());
    }
    for (graph_key, property) in OTHER_SCALARS {
        if let Some(value) = other_value(card, property) {
            body[*graph_key] = json!(value);
        }
    }
    if let Some(children) = other_value(card, "X-CHILDREN") {
        let list: Vec<&str> = children.split(',').map(str::trim).filter(|child| !child.is_empty()).collect();
        body["children"] = json!(list);
    }

    body
}

/// Builds the [`VCard`] for a Graph contact `value`, whose `id` becomes the
/// card's `UID` (identity that survives a round trip and matches the write
/// path's `href`).
fn card_from_graph(value: &Value, id: &str) -> VCard {
    VCard {
        version: "4.0".to_string(),
        kind: None,
        uid: Some(id.to_string()),
        full_name: non_empty_str(value, "displayName"),
        name: name_from_graph(value),
        organization: organization_from_graph(value),
        title: non_empty_str(value, "jobTitle"),
        file_as: non_empty_str(value, "fileAs"),
        photo: None,
        emails: emails_from_graph(value),
        telephones: phones_from_graph(value),
        addresses: addresses_from_graph(value),
        urls: business_home_page(value).into_iter().collect(),
        impp: im_addresses_from_graph(value),
        note: non_empty_str(value, "personalNotes"),
        birthday: birthday_from_graph(value),
        anniversary: None,
        categories: string_array(value, "categories"),
        other: other_scalars_from_graph(value),
    }
}

fn name_from_graph(value: &Value) -> Option<Name> {
    let name = Name {
        family: str_or_empty(value, "surname"),
        given: str_or_empty(value, "givenName"),
        additional: str_or_empty(value, "middleName"),
        // Outlook's name "Title" (Mr./Ms./Dr.), not the job title.
        prefix: str_or_empty(value, "title"),
        suffix: str_or_empty(value, "generation"),
    };
    if [&name.family, &name.given, &name.additional, &name.prefix, &name.suffix].iter().all(|part| part.is_empty()) {
        None
    } else {
        Some(name)
    }
}

fn organization_from_graph(value: &Value) -> Option<Vec<String>> {
    let company = str_or_empty(value, "companyName");
    let department = str_or_empty(value, "department");
    if company.is_empty() && department.is_empty() {
        None
    } else {
        Some(vec![company, department])
    }
}

fn emails_from_graph(value: &Value) -> Vec<EmailField> {
    value
        .get("emailAddresses")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get("address").and_then(Value::as_str))
        .map(str::trim)
        .filter(|address| !address.is_empty())
        .map(|address| EmailField {
            types: Vec::new(),
            address: address.to_string(),
        })
        .collect()
}

fn phones_from_graph(value: &Value) -> Vec<TelephoneField> {
    let mut phones = Vec::new();
    for (key, label) in [("businessPhones", "work"), ("homePhones", "home")] {
        for number in string_array(value, key) {
            if !number.trim().is_empty() {
                phones.push(TelephoneField {
                    types: vec![label.to_string()],
                    number,
                });
            }
        }
    }
    if let Some(mobile) = non_empty_str(value, "mobilePhone") {
        phones.push(TelephoneField {
            types: vec!["cell".to_string()],
            number: mobile,
        });
    }
    phones
}

fn addresses_from_graph(value: &Value) -> Vec<AddressField> {
    let mut addresses = Vec::new();
    for (key, label) in [("businessAddress", "work"), ("homeAddress", "home"), ("otherAddress", "other")] {
        if let Some(address) = address_from_graph(value.get(key), label) {
            addresses.push(address);
        }
    }
    addresses
}

/// One Graph `physicalAddress` as an [`AddressField`], or `None` when every
/// part is empty (Graph often returns an all-empty address block).
fn address_from_graph(value: Option<&Value>, label: &str) -> Option<AddressField> {
    let value = value?;
    if !value.is_object() {
        return None;
    }
    let address = AddressField {
        types: vec![label.to_string()],
        po_box: str_or_empty(value, "postOfficeBox"),
        extended: String::new(),
        street: str_or_empty(value, "street"),
        locality: str_or_empty(value, "city"),
        region: str_or_empty(value, "state"),
        postal_code: str_or_empty(value, "postalCode"),
        country: str_or_empty(value, "countryOrRegion"),
        label: None,
    };
    let empty = [&address.street, &address.locality, &address.region, &address.postal_code, &address.country, &address.po_box]
        .iter()
        .all(|part| part.is_empty());
    if empty {
        None
    } else {
        Some(address)
    }
}

/// One [`AddressField`] as a Graph `physicalAddress`, or `None` when every
/// part is empty.
fn address_to_graph(address: &AddressField) -> Option<Value> {
    let mut value = json!({});
    let mut any = false;
    for (key, text) in [
        ("street", &address.street),
        ("city", &address.locality),
        ("state", &address.region),
        ("postalCode", &address.postal_code),
        ("countryOrRegion", &address.country),
        ("postOfficeBox", &address.po_box),
    ] {
        if !text.trim().is_empty() {
            value[key] = json!(text.trim());
            any = true;
        }
    }
    any.then_some(value)
}

fn im_addresses_from_graph(value: &Value) -> Vec<ImppField> {
    string_array(value, "imAddresses")
        .into_iter()
        .filter(|uri| !uri.trim().is_empty())
        .map(|uri| ImppField { types: Vec::new(), uri })
        .collect()
}

fn birthday_from_graph(value: &Value) -> Option<Birthday> {
    parse_birthday(value.get("birthday").and_then(Value::as_str)?)
}

/// Parses Graph's `birthday` string - `YYYY-MM-DD` (optionally with a time
/// suffix) or vCard's yearless `--MM-DD`. A yearless value sets `omit_year`.
fn parse_birthday(raw: &str) -> Option<Birthday> {
    let raw = raw.trim();
    if let Some(rest) = raw.strip_prefix("--") {
        let date = NaiveDate::parse_from_str(&format!("2000-{rest}"), "%Y-%m-%d").ok()?;
        return Some(Birthday { date, omit_year: true });
    }
    let date_part = raw.split(['T', ' ']).next().unwrap_or(raw);
    let date = NaiveDate::parse_from_str(date_part, "%Y-%m-%d").ok()?;
    Some(Birthday { date, omit_year: false })
}

fn business_home_page(value: &Value) -> Option<String> {
    non_empty_str(value, "businessHomePage")
}

fn other_scalars_from_graph(value: &Value) -> Vec<OtherProperty> {
    let mut other = Vec::new();
    for (graph_key, property) in OTHER_SCALARS {
        if let Some(text) = non_empty_str(value, graph_key) {
            other.push(OtherProperty {
                name: property.to_string(),
                params: Vec::new(),
                value: text,
            });
        }
    }
    let children = string_array(value, "children");
    if !children.is_empty() {
        other.push(OtherProperty {
            name: "X-CHILDREN".to_string(),
            params: Vec::new(),
            value: children.join(", "),
        });
    }
    other
}

/// Maps a `/me/people` result (`person`) to a [`DirectoryPerson`], or `None`
/// when it carries no scored e-mail address.
pub fn person_from_graph(value: &Value) -> Option<DirectoryPerson> {
    let address = value
        .get("scoredEmailAddresses")
        .and_then(Value::as_array)
        .and_then(|addresses| addresses.first())
        .and_then(|entry| entry.get("address"))
        .and_then(Value::as_str)
        .filter(|address| !address.trim().is_empty())?;
    Some(DirectoryPerson {
        id: value.get("id").and_then(Value::as_str).map(str::to_string),
        name: non_empty_str(value, "displayName"),
        address: address.to_string(),
        title: None,
        company: None,
    })
}

/// Maps a `/users` result to a [`DirectoryPerson`], or `None` when it has no
/// e-mail address (`mail`, falling back to `userPrincipalName`).
pub fn user_from_graph(value: &Value) -> Option<DirectoryPerson> {
    let address = non_empty_str(value, "mail").or_else(|| non_empty_str(value, "userPrincipalName"))?;
    Some(DirectoryPerson {
        id: value.get("id").and_then(Value::as_str).map(str::to_string),
        name: non_empty_str(value, "displayName"),
        address,
        title: non_empty_str(value, "jobTitle"),
        company: non_empty_str(value, "companyName"),
    })
}

fn other_value(card: &VCard, name: &str) -> Option<String> {
    card.other
        .iter()
        .find(|property| property.name.eq_ignore_ascii_case(name))
        .map(|property| property.value.clone())
        .filter(|value| !value.trim().is_empty())
}

fn change_key(value: &Value) -> Option<String> {
    value.get("changeKey").and_then(Value::as_str).map(str::to_string)
}

fn non_empty_str(value: &Value, key: &str) -> Option<String> {
    value.get(key).and_then(Value::as_str).filter(|text| !text.is_empty()).map(str::to_string)
}

fn str_or_empty(value: &Value, key: &str) -> String {
    value.get(key).and_then(Value::as_str).unwrap_or_default().to_string()
}

fn string_array(value: &Value, key: &str) -> Vec<String> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect())
        .unwrap_or_default()
}

/// Sets `body[key] = value` only when `value` is non-blank, so an empty model
/// field never blanks a value Outlook holds.
fn set(body: &mut Value, key: &str, value: &str) {
    if !value.trim().is_empty() {
        body[key] = json!(value.trim());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_contact() -> Value {
        json!({
            "id": "AAMkAD-contact",
            "changeKey": "ck-1",
            "parentFolderId": "folder-1",
            "displayName": "Ada King",
            "givenName": "Ada",
            "middleName": "M",
            "surname": "King",
            "title": "Dr.",
            "generation": "Jr.",
            "jobTitle": "CTO",
            "initials": "AK",
            "nickName": "Ad",
            "fileAs": "King, Ada",
            "companyName": "Analytical Engines",
            "department": "Research",
            "officeLocation": "Room 4",
            "profession": "Engineer",
            "manager": "Charles Babbage",
            "assistantName": "Mary",
            "spouseName": "William King",
            "children": ["Byron", "Annabella"],
            "emailAddresses": [{ "name": "Ada King", "address": "ada@example.com" }, { "address": "ada.king@work.example" }],
            "businessPhones": ["+44 20 7946 0001"],
            "homePhones": ["+44 20 7946 0002"],
            "mobilePhone": "+44 7700 900123",
            "homeAddress": { "street": "12 Baker St", "city": "London", "postalCode": "NW1", "countryOrRegion": "UK" },
            "businessAddress": { "street": "", "city": "", "state": "", "postalCode": "", "countryOrRegion": "" },
            "otherAddress": { "postOfficeBox": "PO 9" },
            "imAddresses": ["sip:ada@example.com"],
            "personalNotes": "Met at the conference.",
            "categories": ["Projects"],
            "birthday": "1815-12-10T00:00:00Z",
            "businessHomePage": "https://ada.example.com"
        })
    }

    #[test]
    fn contact_from_graph_maps_the_full_card() {
        let contact = contact_from_graph(&sample_contact()).expect("a contact");
        assert_eq!(contact.id, "AAMkAD-contact");
        assert_eq!(contact.etag.as_deref(), Some("ck-1"));
        let card = contact.card;
        assert_eq!(card.uid.as_deref(), Some("AAMkAD-contact"));
        assert_eq!(card.full_name.as_deref(), Some("Ada King"));
        let name = card.name.expect("a name");
        assert_eq!((name.given.as_str(), name.family.as_str(), name.additional.as_str()), ("Ada", "King", "M"));
        assert_eq!((name.prefix.as_str(), name.suffix.as_str()), ("Dr.", "Jr."));
        assert_eq!(card.title.as_deref(), Some("CTO"));
        assert_eq!(card.file_as.as_deref(), Some("King, Ada"));
        assert_eq!(card.organization.as_deref(), Some(["Analytical Engines".to_string(), "Research".to_string()].as_slice()));
        assert_eq!(
            card.emails.iter().map(|e| e.address.as_str()).collect::<Vec<_>>(),
            vec!["ada@example.com", "ada.king@work.example"]
        );
        assert_eq!(card.telephones.len(), 3);
        assert!(card.telephones.iter().any(|p| p.number == "+44 7700 900123" && p.types == ["cell"]));
        // The all-empty business address is dropped; home and other survive.
        assert_eq!(card.addresses.len(), 2);
        assert!(card.addresses.iter().any(|a| a.types == ["home"] && a.locality == "London"));
        assert!(card.addresses.iter().any(|a| a.types == ["other"] && a.po_box == "PO 9"));
        assert_eq!(card.urls, vec!["https://ada.example.com".to_string()]);
        assert_eq!(card.impp.len(), 1);
        assert_eq!(card.impp[0].uri, "sip:ada@example.com");
        assert_eq!(card.note.as_deref(), Some("Met at the conference."));
        assert_eq!(card.categories, vec!["Projects".to_string()]);
        let birthday = card.birthday.expect("a birthday");
        assert_eq!(birthday.date, NaiveDate::from_ymd_opt(1815, 12, 10).unwrap());
        assert!(!birthday.omit_year);
        // Scalar fields with no vCard home round-trip through `other`.
        for (_, property) in OTHER_SCALARS {
            assert!(card.other.iter().any(|p| p.name == *property), "{property} missing");
        }
        assert!(card.other.iter().any(|p| p.name == "X-CHILDREN" && p.value == "Byron, Annabella"));
    }

    #[test]
    fn contact_to_graph_writes_the_fields_it_reads_back() {
        let card = contact_from_graph(&sample_contact()).unwrap().card;
        let body = contact_to_graph(&card);
        assert_eq!(body["displayName"], json!("Ada King"));
        assert_eq!(body["givenName"], json!("Ada"));
        assert_eq!(body["surname"], json!("King"));
        assert_eq!(body["title"], json!("Dr."));
        assert_eq!(body["jobTitle"], json!("CTO"));
        assert_eq!(body["companyName"], json!("Analytical Engines"));
        assert_eq!(body["department"], json!("Research"));
        assert_eq!(body["emailAddresses"], json!([{ "address": "ada@example.com" }, { "address": "ada.king@work.example" }]));
        assert_eq!(body["businessPhones"], json!(["+44 20 7946 0001"]));
        assert_eq!(body["homePhones"], json!(["+44 20 7946 0002"]));
        assert_eq!(body["mobilePhone"], json!("+44 7700 900123"));
        assert_eq!(body["homeAddress"]["city"], json!("London"));
        assert_eq!(body["otherAddress"]["postOfficeBox"], json!("PO 9"));
        assert_eq!(body["imAddresses"], json!(["sip:ada@example.com"]));
        assert_eq!(body["personalNotes"], json!("Met at the conference."));
        assert_eq!(body["categories"], json!(["Projects"]));
        assert_eq!(body["birthday"], json!("1815-12-10"));
        assert_eq!(body["businessHomePage"], json!("https://ada.example.com"));
        assert_eq!(body["officeLocation"], json!("Room 4"));
        assert_eq!(body["spouseName"], json!("William King"));
        assert_eq!(body["children"], json!(["Byron", "Annabella"]));
    }

    #[test]
    fn an_empty_model_field_is_not_written() {
        let card = VCard {
            version: "4.0".to_string(),
            kind: None,
            uid: Some("u1".to_string()),
            full_name: Some("Solo".to_string()),
            name: None,
            organization: None,
            title: Some("  ".to_string()),
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
        };
        let body = contact_to_graph(&card);
        assert_eq!(body["displayName"], json!("Solo"));
        assert!(body.get("jobTitle").is_none(), "a blank title must not be written");
        assert!(body.get("birthday").is_none());
        assert_eq!(body["emailAddresses"], json!([]));
    }

    #[test]
    fn removed_delta_entries_map_to_their_id() {
        let removed = json!({ "id": "gone-1", "@removed": { "reason": "deleted" } });
        assert_eq!(removed_id(&removed).as_deref(), Some("gone-1"));
        assert!(contact_from_graph(&removed).is_none());
        assert!(removed_id(&sample_contact()).is_none());
    }

    #[test]
    fn birthday_parses_yearless_forms() {
        assert!(parse_birthday("--12-10").unwrap().omit_year);
        assert_eq!(parse_birthday("--12-10").unwrap().date, NaiveDate::from_ymd_opt(2000, 12, 10).unwrap());
        assert!(!parse_birthday("1815-12-10").unwrap().omit_year);
        assert!(parse_birthday("not a date").is_none());
    }

    #[test]
    fn the_captured_fixture_round_trips_through_the_mapper() {
        let page: Value = serde_json::from_str(include_str!("../../../../test-fixtures/outlook/contacts.json")).unwrap();
        let contacts: Vec<GraphContact> = page.get("value").and_then(Value::as_array).unwrap().iter().filter_map(contact_from_graph).collect();
        assert_eq!(contacts.len(), 2, "the fixture carries two contacts");

        let alex = &contacts[0].card;
        assert_eq!(alex.full_name.as_deref(), Some("Alex Wilber"));
        assert_eq!(alex.title.as_deref(), Some("Marketing Assistant"));
        assert_eq!(alex.emails.len(), 2);
        assert_eq!(alex.categories, vec!["Customers".to_string(), "Marketing".to_string()]);
        assert!(alex.birthday.is_some_and(|b| !b.omit_year));
        // The yearless birthday fixture keeps the omit-year flag.
        assert!(contacts[1].card.birthday.is_some_and(|b| b.omit_year));

        // A write reproduces the fields the fixture holds, so a Lookout edit
        // can't drop them.
        let body = contact_to_graph(alex);
        assert_eq!(body["displayName"], json!("Alex Wilber"));
        assert_eq!(body["jobTitle"], json!("Marketing Assistant"));
        assert_eq!(body["title"], json!("Mr."));
        assert_eq!(body["businessHomePage"], json!("https://www.contoso.com"));
        assert_eq!(body["birthday"], json!("1985-04-23"));
    }

    #[test]
    fn the_captured_folder_fixture_maps_every_folder() {
        let page: Value = serde_json::from_str(include_str!("../../../../test-fixtures/outlook/contact-folders.json")).unwrap();
        let folders: Vec<ContactFolder> = page.get("value").and_then(Value::as_array).unwrap().iter().filter_map(contact_folder_from_graph).collect();
        assert_eq!(folders.len(), 2);
        assert_eq!(folders[1].display_name, "Contoso Partners");
    }

    #[test]
    fn contact_folder_from_graph_reads_id_and_name() {
        let folder = contact_folder_from_graph(&json!({ "id": "f1", "displayName": "Work" })).unwrap();
        assert_eq!(folder.id, "f1");
        assert_eq!(folder.display_name, "Work");
        // A folder with no name falls back rather than rendering blank.
        assert_eq!(contact_folder_from_graph(&json!({ "id": "f2" })).unwrap().display_name, "Contacts");
        assert!(contact_folder_from_graph(&json!({ "displayName": "No id" })).is_none());
    }
}
