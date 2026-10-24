//! Duplicate-contact matching and field-level merging (OUTLOOK_PARITY.md
//! item 4.6 / P-09).
//!
//! Everything here is pure: the app passes in the vCards it has already
//! synced, renders [`MergeFields`] as a form, and writes the card returned by
//! [`merge_cards`] through its normal contact command channel. The server
//! object model (address-book hrefs, etags, Graph ids) never enters this
//! module.
//!
//! Matching is deliberately conservative:
//!
//! - a shared email address, phone number or UID is a strong match;
//! - a name alone connects only cards that have no email and no phone, so two
//!   complete "John Smith" contacts are not grouped just because they share a
//!   common name, and a bare third copy cannot glue them together.
//!
//! A group is an equivalence class, so a chain (A shares an email with B, B
//! shares a phone with C) becomes one group. The UI is a review step, not an
//! automatic merge.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::hash_map::DefaultHasher;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::hash::{Hash, Hasher};

use crate::vcard::{AddressField, Birthday, EmailField, ImppField, OtherProperty, Photo, TelephoneField, VCard};

/// Which field connected two contacts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum MatchKind {
    Email,
    Phone,
    Name,
    Uid,
}

/// One reason a group is considered duplicate. `value` is the first value
/// that produced the edge, in its original form (not the normalized key), so
/// the UI can show it to the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MatchReason {
    pub kind: MatchKind,
    pub value: String,
}

/// Indices into the slice passed to [`duplicate_groups`], plus the reasons
/// those cards are connected.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DuplicateGroup {
    pub indices: Vec<usize>,
    pub reasons: Vec<MatchReason>,
}

/// A field where exactly one source card's value is kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MergeScalar {
    Name,
    Organization,
    Title,
    FileAs,
    Birthday,
    Anniversary,
    Note,
    Photo,
}

/// A repeatable field where any number of values can be kept.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum MergeList {
    Email,
    Phone,
    Address,
    Url,
    Impp,
    Category,
    Custom,
}

/// A candidate value for a scalar field. `source` is the card index whose
/// value is copied; `sources` lists every card that carried the same value
/// (after normalization) so the UI can say "from X +1".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScalarChoice {
    pub source: usize,
    /// A stable, normalized key used only while building the candidate list.
    pub key: String,
    pub text: String,
    pub sources: Vec<usize>,
}

/// A candidate value for a repeatable field.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListChoice {
    pub key: String,
    pub text: String,
    pub sources: Vec<usize>,
}

/// Every field value found across a group, already deduplicated and grouped
/// by field. The merge dialog renders these; the host never has to re-derive
/// vCard comparisons itself.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeFields {
    pub scalars: BTreeMap<MergeScalar, Vec<ScalarChoice>>,
    pub lists: BTreeMap<MergeList, Vec<ListChoice>>,
}

/// The user's field-level choices for one merge.
///
/// A missing scalar field means "leave the keeper's value alone"; an explicit
/// `None` means "omit it". A missing list field likewise leaves the keeper's
/// values alone, while an empty set deliberately clears it. The dialog always
/// populates every list field it displays.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MergeChoices {
    pub scalars: BTreeMap<MergeScalar, Option<usize>>,
    pub lists: BTreeMap<MergeList, BTreeSet<String>>,
    pub combine_notes: bool,
}

impl MergeFields {
    /// Builds the candidate lists from a group. Order is source order, so the
    /// keeper's cards (which the caller normally puts first) lead each list.
    pub fn build(cards: &[&VCard]) -> MergeFields {
        let mut fields = MergeFields::default();
        for (source, card) in cards.iter().enumerate() {
            for field in scalar_fields() {
                let Some(value) = scalar_value(card, field) else { continue };
                push_scalar(&mut fields, field, source, value);
            }
            for field in list_fields() {
                for value in list_values(card, field) {
                    push_list(&mut fields, field, source, value);
                }
            }
        }
        fields
    }
}

/// Groups cards that are likely the same contact. The returned indices refer
/// to `cards`; contact lists (`KIND:group`) are ignored.
pub fn duplicate_groups(cards: &[&VCard]) -> Vec<DuplicateGroup> {
    if cards.len() < 2 {
        return Vec::new();
    }

    let mut union = UnionFind::new(cards.len());
    let mut reasons = Vec::new();
    let mut email_owner: HashMap<String, usize> = HashMap::new();
    let mut phone_owner: HashMap<String, usize> = HashMap::new();
    let mut uid_owner: HashMap<String, usize> = HashMap::new();
    let mut name_buckets: HashMap<String, Vec<usize>> = HashMap::new();

    for (index, card) in cards.iter().enumerate() {
        if card.is_contact_list() {
            continue;
        }

        for email in &card.emails {
            let Some(key) = normalize_email(&email.address) else { continue };
            match email_owner.get(&key) {
                Some(&owner) if owner != index => {
                    union.union(owner, index);
                    reasons.push((
                        owner,
                        index,
                        MatchReason {
                            kind: MatchKind::Email,
                            value: email.address.trim().to_string(),
                        },
                    ));
                }
                Some(_) => {}
                None => {
                    email_owner.insert(key, index);
                }
            }
        }

        for phone in &card.telephones {
            let keys = phone_match_keys(&phone.number);
            for key in keys {
                match phone_owner.get(&key) {
                    Some(&owner) if owner != index => {
                        union.union(owner, index);
                        reasons.push((
                            owner,
                            index,
                            MatchReason {
                                kind: MatchKind::Phone,
                                value: phone.number.trim().to_string(),
                            },
                        ));
                    }
                    Some(_) => {}
                    None => {
                        phone_owner.insert(key, index);
                    }
                }
            }
        }

        if let Some(uid) = card.uid.as_deref().map(str::trim).filter(|uid| !uid.is_empty()) {
            match uid_owner.get(uid) {
                Some(&owner) if owner != index => {
                    union.union(owner, index);
                    reasons.push((
                        owner,
                        index,
                        MatchReason {
                            kind: MatchKind::Uid,
                            value: uid.to_string(),
                        },
                    ));
                }
                Some(_) => {}
                None => {
                    uid_owner.insert(uid.to_string(), index);
                }
            }
        }

        if let Some(name) = name_key(card) {
            name_buckets.entry(name).or_default().push(index);
        }
    }

    // Name-only matches are limited to cards with no email and no phone. A
    // complete card with the same common name stays out, so a bare third copy
    // cannot glue two different people together through the name bucket.
    for indices in name_buckets.into_values() {
        let bare: Vec<usize> = indices.into_iter().filter(|&index| !has_contact_info(cards[index])).collect();
        if bare.len() < 2 {
            continue;
        }
        let first = bare[0];
        let value = name_text(cards[first]).unwrap_or_default();
        for &index in &bare[1..] {
            union.union(first, index);
            reasons.push((
                first,
                index,
                MatchReason {
                    kind: MatchKind::Name,
                    value: value.clone(),
                },
            ));
        }
    }

    let mut members: HashMap<usize, Vec<usize>> = HashMap::new();
    for index in 0..cards.len() {
        members.entry(union.find(index)).or_default().push(index);
    }

    let mut groups: Vec<DuplicateGroup> = members
        .into_values()
        .filter(|indices| indices.len() > 1)
        .map(|mut indices| {
            indices.sort_unstable();
            let roots: HashSet<usize> = indices.iter().map(|&index| union.find(index)).collect();
            let mut group_reasons: Vec<MatchReason> = reasons
                .iter()
                .filter(|(a, b, _)| roots.contains(&union.find(*a)) && roots.contains(&union.find(*b)))
                .map(|(_, _, reason)| reason.clone())
                .collect();
            group_reasons.sort_by(|a, b| a.kind.cmp(&b.kind).then_with(|| a.value.cmp(&b.value)));
            group_reasons.dedup();
            DuplicateGroup { indices, reasons: group_reasons }
        })
        .collect();
    groups.sort_by_key(|group| group.indices.first().copied().unwrap_or(usize::MAX));
    groups
}

/// The keeper's default field choices: its own value where it has one, else
/// the first non-empty value in source order; every repeatable value is kept.
pub fn default_choices(cards: &[&VCard], keeper: usize) -> MergeChoices {
    let mut choices = MergeChoices {
        combine_notes: true,
        ..MergeChoices::default()
    };
    if cards.is_empty() {
        return choices;
    }
    let keeper = keeper.min(cards.len() - 1);
    for field in scalar_fields() {
        if let Some(source) = scalar_source(cards, keeper, field) {
            choices.scalars.insert(field, Some(source));
        }
    }
    for (field, candidates) in MergeFields::build(cards).lists {
        choices.lists.insert(field, candidates.into_iter().map(|candidate| candidate.key).collect());
    }
    choices
}

/// Builds the merged card. The keeper supplies the UID, version, kind and any
/// scalar field the caller did not choose; chosen repeatable fields are
/// unioned and deduplicated. Returns `None` for an empty group or a keeper
/// index outside it.
pub fn merge_cards(cards: &[&VCard], keeper: usize, choices: &MergeChoices) -> Option<VCard> {
    let keeper_card = cards.get(keeper)?;
    let mut merged = (*keeper_card).clone();
    let ordered: Vec<usize> = std::iter::once(keeper).chain((0..cards.len()).filter(|index| *index != keeper)).collect();

    if let Some(source) = choices.scalars.get(&MergeScalar::Name) {
        match source {
            Some(source) if *source < cards.len() => {
                merged.full_name = cards[*source].full_name.clone();
                merged.name = cards[*source].name.clone();
            }
            _ => {
                merged.full_name = None;
                merged.name = None;
            }
        }
    }
    copy_scalar(&mut merged, cards, choices, MergeScalar::Organization, |merged, source| {
        merged.organization = source.organization.clone();
    });
    copy_scalar(&mut merged, cards, choices, MergeScalar::Title, |merged, source| {
        merged.title = source.title.clone();
    });
    copy_scalar(&mut merged, cards, choices, MergeScalar::FileAs, |merged, source| {
        merged.file_as = source.file_as.clone();
    });
    copy_scalar(&mut merged, cards, choices, MergeScalar::Birthday, |merged, source| {
        merged.birthday = source.birthday;
    });
    copy_scalar(&mut merged, cards, choices, MergeScalar::Anniversary, |merged, source| {
        merged.anniversary = source.anniversary;
    });
    copy_scalar(&mut merged, cards, choices, MergeScalar::Photo, |merged, source| {
        merged.photo = source.photo.clone();
    });

    if choices.combine_notes {
        let mut notes: Vec<String> = Vec::new();
        for &index in &ordered {
            if let Some(note) = cards[index].note.as_deref().map(str::trim).filter(|note| !note.is_empty()) {
                if !notes.iter().any(|existing| existing == note) {
                    notes.push(note.to_string());
                }
            }
        }
        merged.note = (!notes.is_empty()).then(|| notes.join("\n\n"));
    } else if let Some(source) = choices.scalars.get(&MergeScalar::Note) {
        match source {
            Some(source) if *source < cards.len() => merged.note = cards[*source].note.clone(),
            _ => merged.note = None,
        }
    }

    if choices.lists.contains_key(&MergeList::Email) {
        merged.emails = merge_emails(cards, &ordered, choices);
    }
    if choices.lists.contains_key(&MergeList::Phone) {
        merged.telephones = merge_phones(cards, &ordered, choices);
    }
    if choices.lists.contains_key(&MergeList::Address) {
        merged.addresses = merge_addresses(cards, &ordered, choices);
    }
    if choices.lists.contains_key(&MergeList::Url) {
        merged.urls = merge_urls(cards, &ordered, choices);
    }
    if choices.lists.contains_key(&MergeList::Impp) {
        merged.impp = merge_impp(cards, &ordered, choices);
    }
    if choices.lists.contains_key(&MergeList::Category) {
        merged.categories = merge_categories(cards, &ordered, choices);
    }
    if choices.lists.contains_key(&MergeList::Custom) {
        merged.other = merge_custom(cards, &ordered, choices);
    }

    // An update keeps the server object's identity; a card that somehow lost
    // its UID still gets one so a later export/import cannot collide.
    if merged.uid.as_deref().map(str::trim).is_none_or(|uid| uid.is_empty()) {
        merged.uid = Some(uuid::Uuid::new_v4().to_string());
    }
    Some(merged)
}

/// A rough "how much is filled in" score. The dialog defaults its keeper to
/// the most complete card in a group.
pub fn completeness(card: &VCard) -> usize {
    usize::from(name_text(card).is_some())
        + usize::from(card.organization.as_ref().is_some_and(|org| !org.is_empty()))
        + usize::from(card.title.as_deref().is_some_and(|title| !title.trim().is_empty()))
        + usize::from(card.file_as.as_deref().is_some_and(|file_as| !file_as.trim().is_empty()))
        + usize::from(card.birthday.is_some())
        + usize::from(card.anniversary.is_some())
        + usize::from(card.note.as_deref().is_some_and(|note| !note.trim().is_empty()))
        + usize::from(card.photo.is_some())
        + card.emails.len()
        + card.telephones.len()
        + card.addresses.len()
        + card.urls.len()
        + card.impp.len()
        + card.categories.len()
        + card.other.len()
}

/// Trims and lowercases an email address for comparison. Accepts a `mailto:`
/// prefix and a display-name form (`Ada <ada@example.com>`).
pub fn normalize_email(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lowered = trimmed.to_ascii_lowercase();
    let without_mailto = lowered.strip_prefix("mailto:").unwrap_or(&lowered);
    let address = if let Some(start) = without_mailto.find('<') {
        without_mailto[start + 1..].split('>').next().unwrap_or("").trim()
    } else {
        without_mailto.trim()
    };
    // A card can carry a malformed value (`EMAIL:not-an-address`). It is not a
    // duplicate key, and it must not make a card look "complete" to the
    // name-only rule either.
    (!address.is_empty() && address.contains('@')).then(|| address.to_string())
}

/// Digits-only phone key. Punctuation, spaces and a `tel:` prefix disappear;
/// fewer than seven digits is not enough to identify a person.
pub fn normalize_phone(value: &str) -> Option<String> {
    let digits: String = value.chars().filter(|character| character.is_ascii_digit()).collect();
    if digits.len() < 7 {
        return None;
    }
    let trimmed = digits.trim_start_matches('0');
    Some(if trimmed.is_empty() { digits } else { trimmed.to_string() })
}

/// Additional match keys for a phone number: the full digits and, for longer
/// numbers, the trailing nine digits. That lets `020 7946 0018` match
/// `+44 20 7946 0018` without treating a seven-digit extension as a match for
/// every number that ends with it.
fn phone_match_keys(value: &str) -> Vec<String> {
    let Some(full) = normalize_phone(value) else { return Vec::new() };
    let mut keys = vec![full.clone()];
    if full.len() > 9 {
        let tail = full[full.len() - 9..].to_string();
        if tail != full {
            keys.push(tail);
        }
    }
    keys
}

/// The contact's real name (`FN`, else the structured `N` parts), ignoring
/// the email fallback used by [`VCard::display_name`].
fn name_text(card: &VCard) -> Option<String> {
    if let Some(full_name) = card.full_name.as_deref().map(str::trim).filter(|name| !name.is_empty()) {
        return Some(full_name.to_string());
    }
    let name = card.name.as_ref()?;
    let parts = [&name.prefix, &name.given, &name.additional, &name.family, &name.suffix]
        .into_iter()
        .map(|part| part.trim())
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// Token-sorted lowercase name key, so `Doe, Jane` and `Jane Doe` compare
/// equal. Diacritics are left alone rather than guessed at.
fn name_key(card: &VCard) -> Option<String> {
    let text = name_text(card)?;
    let mut tokens: Vec<String> = text
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| !token.is_empty())
        .map(str::to_lowercase)
        .collect();
    if tokens.is_empty() {
        return None;
    }
    tokens.sort();
    tokens.dedup();
    Some(tokens.join(" "))
}

fn has_contact_info(card: &VCard) -> bool {
    card.emails.iter().any(|email| normalize_email(&email.address).is_some()) || card.telephones.iter().any(|phone| normalize_phone(&phone.number).is_some())
}

fn scalar_fields() -> [MergeScalar; 8] {
    [
        MergeScalar::Name,
        MergeScalar::Organization,
        MergeScalar::Title,
        MergeScalar::FileAs,
        MergeScalar::Birthday,
        MergeScalar::Anniversary,
        MergeScalar::Note,
        MergeScalar::Photo,
    ]
}

fn list_fields() -> [MergeList; 7] {
    [
        MergeList::Email,
        MergeList::Phone,
        MergeList::Address,
        MergeList::Url,
        MergeList::Impp,
        MergeList::Category,
        MergeList::Custom,
    ]
}

struct ScalarValue {
    key: String,
    text: String,
}

fn scalar_value(card: &VCard, field: MergeScalar) -> Option<ScalarValue> {
    match field {
        MergeScalar::Name => {
            let text = name_text(card)?;
            Some(ScalarValue { key: name_key(card)?, text })
        }
        MergeScalar::Organization => {
            let parts = card
                .organization
                .as_ref()?
                .iter()
                .map(|part| part.trim())
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>();
            if parts.is_empty() {
                return None;
            }
            Some(ScalarValue {
                key: parts.iter().map(|part| part.to_lowercase()).collect::<Vec<_>>().join("|"),
                text: parts.join(" / "),
            })
        }
        MergeScalar::Title => non_empty_scalar(card.title.as_deref()),
        MergeScalar::FileAs => non_empty_scalar(card.file_as.as_deref()),
        MergeScalar::Birthday => card.birthday.map(birthday_value),
        MergeScalar::Anniversary => card.anniversary.map(birthday_value),
        MergeScalar::Note => non_empty_scalar(card.note.as_deref()),
        MergeScalar::Photo => card.photo.as_ref().map(|photo| ScalarValue {
            // Each distinct photo is its own candidate: two embedded images
            // with the same media type are not interchangeable. Hash instead
            // of formatting the bytes into the key.
            key: photo_key(photo),
            text: photo_text(photo),
        }),
    }
}

fn birthday_value(value: Birthday) -> ScalarValue {
    let text = if value.omit_year {
        value.date.format("%m-%d").to_string()
    } else {
        value.date.format("%Y-%m-%d").to_string()
    };
    ScalarValue {
        key: format!("{}:{}", text, value.omit_year),
        text,
    }
}

fn non_empty_scalar(value: Option<&str>) -> Option<ScalarValue> {
    let text = value.map(str::trim).filter(|value| !value.is_empty())?;
    Some(ScalarValue {
        key: text.to_lowercase(),
        text: text.to_string(),
    })
}

fn photo_key(photo: &Photo) -> String {
    let mut hasher = DefaultHasher::new();
    photo.media_type.hash(&mut hasher);
    photo.data.hash(&mut hasher);
    photo.uri.hash(&mut hasher);
    format!("photo:{}:{:x}", photo.data.as_ref().map(Vec::len).unwrap_or(0), hasher.finish())
}

fn photo_text(photo: &Photo) -> String {
    if photo.data.is_some() {
        "Embedded photo".to_string()
    } else if let Some(uri) = photo.uri.as_deref().map(str::trim).filter(|uri| !uri.is_empty()) {
        uri.to_string()
    } else {
        "Photo".to_string()
    }
}

fn list_values(card: &VCard, field: MergeList) -> Vec<(String, String)> {
    match field {
        MergeList::Email => card
            .emails
            .iter()
            .filter_map(|email| normalize_email(&email.address).map(|key| (key, email.address.trim().to_string())))
            .collect(),
        MergeList::Phone => card
            .telephones
            .iter()
            .filter_map(|phone| normalize_phone(&phone.number).map(|key| (key, phone.number.trim().to_string())))
            .collect(),
        MergeList::Address => card
            .addresses
            .iter()
            .filter_map(|address| address_key(address).map(|key| (key, address_text(address))))
            .collect(),
        MergeList::Url => card
            .urls
            .iter()
            .map(|url| url.trim())
            .filter(|url| !url.is_empty())
            .map(|url| (url.to_lowercase(), url.to_string()))
            .collect(),
        MergeList::Impp => card
            .impp
            .iter()
            .map(|impp| impp.uri.trim())
            .filter(|uri| !uri.is_empty())
            .map(|uri| (uri.to_lowercase(), uri.to_string()))
            .collect(),
        MergeList::Category => card
            .categories
            .iter()
            .map(|category| category.trim())
            .filter(|category| !category.is_empty())
            .map(|category| (category.to_lowercase(), category.to_string()))
            .collect(),
        MergeList::Custom => card
            .other
            .iter()
            .filter_map(|property| custom_key(property).map(|key| (key, custom_text(property))))
            .collect(),
    }
}

fn push_scalar(fields: &mut MergeFields, field: MergeScalar, source: usize, value: ScalarValue) {
    let candidates = fields.scalars.entry(field).or_default();
    if let Some(existing) = candidates.iter_mut().find(|candidate| candidate.key == value.key) {
        if !existing.sources.contains(&source) {
            existing.sources.push(source);
        }
        return;
    }
    candidates.push(ScalarChoice {
        source,
        key: value.key,
        text: value.text,
        sources: vec![source],
    });
}

fn push_list(fields: &mut MergeFields, field: MergeList, source: usize, (key, text): (String, String)) {
    let candidates = fields.lists.entry(field).or_default();
    if let Some(existing) = candidates.iter_mut().find(|candidate| candidate.key == key) {
        if !existing.sources.contains(&source) {
            existing.sources.push(source);
        }
        return;
    }
    candidates.push(ListChoice { key, text, sources: vec![source] });
}

fn scalar_source(cards: &[&VCard], keeper: usize, field: MergeScalar) -> Option<usize> {
    if cards.get(keeper).and_then(|card| scalar_value(card, field)).is_some() {
        return Some(keeper);
    }
    cards.iter().position(|card| scalar_value(card, field).is_some())
}

fn copy_scalar(merged: &mut VCard, cards: &[&VCard], choices: &MergeChoices, field: MergeScalar, apply: impl FnOnce(&mut VCard, &VCard)) {
    let Some(source) = choices.scalars.get(&field) else { return };
    match source {
        Some(source) if *source < cards.len() => apply(merged, cards[*source]),
        _ => {
            // Callers that need clearing use the dedicated branches above;
            // for the remaining fields an explicit None clears the value.
            match field {
                MergeScalar::Organization => merged.organization = None,
                MergeScalar::Title => merged.title = None,
                MergeScalar::FileAs => merged.file_as = None,
                MergeScalar::Birthday => merged.birthday = None,
                MergeScalar::Anniversary => merged.anniversary = None,
                MergeScalar::Photo => merged.photo = None,
                MergeScalar::Name | MergeScalar::Note => {}
            }
        }
    }
}

fn merge_emails(cards: &[&VCard], ordered: &[usize], choices: &MergeChoices) -> Vec<EmailField> {
    let Some(selected) = choices.lists.get(&MergeList::Email) else { return Vec::new() };
    let mut merged: Vec<EmailField> = Vec::new();
    for &index in ordered {
        for email in &cards[index].emails {
            let Some(key) = normalize_email(&email.address) else { continue };
            if !selected.contains(&key) {
                continue;
            }
            if let Some(existing) = merged.iter_mut().find(|existing| normalize_email(&existing.address).as_deref() == Some(key.as_str())) {
                union_types(&mut existing.types, &email.types);
            } else {
                merged.push(email.clone());
            }
        }
    }
    merged
}

fn merge_phones(cards: &[&VCard], ordered: &[usize], choices: &MergeChoices) -> Vec<TelephoneField> {
    let Some(selected) = choices.lists.get(&MergeList::Phone) else { return Vec::new() };
    let mut merged: Vec<TelephoneField> = Vec::new();
    for &index in ordered {
        for phone in &cards[index].telephones {
            let Some(key) = normalize_phone(&phone.number) else { continue };
            if !selected.contains(&key) {
                continue;
            }
            if let Some(existing) = merged.iter_mut().find(|existing| normalize_phone(&existing.number).as_deref() == Some(key.as_str())) {
                union_types(&mut existing.types, &phone.types);
            } else {
                merged.push(phone.clone());
            }
        }
    }
    merged
}

fn merge_addresses(cards: &[&VCard], ordered: &[usize], choices: &MergeChoices) -> Vec<AddressField> {
    let Some(selected) = choices.lists.get(&MergeList::Address) else { return Vec::new() };
    let mut merged: Vec<AddressField> = Vec::new();
    for &index in ordered {
        for address in &cards[index].addresses {
            let Some(key) = address_key(address) else { continue };
            if !selected.contains(&key) {
                continue;
            }
            if let Some(existing) = merged.iter_mut().find(|existing| address_key(existing).as_deref() == Some(key.as_str())) {
                union_types(&mut existing.types, &address.types);
            } else {
                merged.push(address.clone());
            }
        }
    }
    merged
}

fn merge_urls(cards: &[&VCard], ordered: &[usize], choices: &MergeChoices) -> Vec<String> {
    let Some(selected) = choices.lists.get(&MergeList::Url) else { return Vec::new() };
    let mut merged = Vec::new();
    for &index in ordered {
        for url in &cards[index].urls {
            let trimmed = url.trim();
            if trimmed.is_empty() || !selected.contains(&trimmed.to_lowercase()) {
                continue;
            }
            if !merged.iter().any(|existing: &String| existing.eq_ignore_ascii_case(trimmed)) {
                merged.push(trimmed.to_string());
            }
        }
    }
    merged
}

fn merge_impp(cards: &[&VCard], ordered: &[usize], choices: &MergeChoices) -> Vec<ImppField> {
    let Some(selected) = choices.lists.get(&MergeList::Impp) else { return Vec::new() };
    let mut merged: Vec<ImppField> = Vec::new();
    for &index in ordered {
        for impp in &cards[index].impp {
            let uri = impp.uri.trim();
            if uri.is_empty() || !selected.contains(&uri.to_lowercase()) {
                continue;
            }
            if let Some(existing) = merged.iter_mut().find(|existing| existing.uri.eq_ignore_ascii_case(uri)) {
                union_types(&mut existing.types, &impp.types);
            } else {
                merged.push(impp.clone());
            }
        }
    }
    merged
}

fn merge_categories(cards: &[&VCard], ordered: &[usize], choices: &MergeChoices) -> Vec<String> {
    let Some(selected) = choices.lists.get(&MergeList::Category) else { return Vec::new() };
    let mut merged: Vec<String> = Vec::new();
    for &index in ordered {
        for category in &cards[index].categories {
            let trimmed = category.trim();
            if trimmed.is_empty() || !selected.contains(&trimmed.to_lowercase()) {
                continue;
            }
            if !merged.iter().any(|existing| existing.eq_ignore_ascii_case(trimmed)) {
                merged.push(trimmed.to_string());
            }
        }
    }
    merged
}

fn merge_custom(cards: &[&VCard], ordered: &[usize], choices: &MergeChoices) -> Vec<OtherProperty> {
    let Some(selected) = choices.lists.get(&MergeList::Custom) else { return Vec::new() };
    let mut merged: Vec<OtherProperty> = Vec::new();
    for &index in ordered {
        for property in &cards[index].other {
            let Some(key) = custom_key(property) else { continue };
            if !selected.contains(&key) {
                continue;
            }
            if !merged.iter().any(|existing| custom_key(existing).as_deref() == Some(key.as_str())) {
                merged.push(property.clone());
            }
        }
    }
    merged
}

fn union_types(existing: &mut Vec<String>, incoming: &[String]) {
    for kind in incoming {
        if !existing.iter().any(|value| value.eq_ignore_ascii_case(kind)) {
            existing.push(kind.clone());
        }
    }
}

fn address_key(address: &AddressField) -> Option<String> {
    let key = [
        &address.po_box,
        &address.extended,
        &address.street,
        &address.locality,
        &address.region,
        &address.postal_code,
        &address.country,
    ]
    .into_iter()
    .map(|part| part.trim().to_lowercase())
    .collect::<Vec<_>>()
    .join("|");
    if !key.replace('|', "").is_empty() {
        return Some(key);
    }
    address.label.as_deref().map(str::trim).filter(|label| !label.is_empty()).map(str::to_lowercase)
}

fn address_text(address: &AddressField) -> String {
    if let Some(label) = address.label.as_deref().map(str::trim).filter(|label| !label.is_empty()) {
        return label.to_string();
    }
    [
        &address.street,
        &address.extended,
        &address.locality,
        &address.region,
        &address.postal_code,
        &address.country,
    ]
    .into_iter()
    .map(|part| part.trim())
    .filter(|part| !part.is_empty())
    .collect::<Vec<_>>()
    .join(", ")
}

fn custom_key(property: &OtherProperty) -> Option<String> {
    let name = property.name.trim();
    let value = property.value.trim();
    if name.is_empty() && value.is_empty() {
        return None;
    }
    Some(format!("{}\u{1f}{}", name.to_lowercase(), value))
}

fn custom_text(property: &OtherProperty) -> String {
    let name = property.name.trim();
    let value = property.value.trim();
    if name.is_empty() {
        value.to_string()
    } else {
        format!("{name}: {value}")
    }
}

/// Minimal union-find; the smaller index wins so group order is stable.
struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(len: usize) -> Self {
        UnionFind { parent: (0..len).collect() }
    }

    fn find(&self, mut index: usize) -> usize {
        while self.parent[index] != index {
            index = self.parent[index];
        }
        index
    }

    fn union(&mut self, a: usize, b: usize) {
        let root_a = self.find(a);
        let root_b = self.find(b);
        if root_a != root_b {
            // Keep the smaller index as the root so group order is stable.
            let (root, child) = if root_a < root_b { (root_a, root_b) } else { (root_b, root_a) };
            self.parent[child] = root;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn card(uid: &str, name: &str, email: Option<&str>, phone: Option<&str>) -> VCard {
        let mut text = format!("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:{uid}\r\nFN:{name}\r\n");
        if let Some(email) = email {
            text.push_str(&format!("EMAIL:{email}\r\n"));
        }
        if let Some(phone) = phone {
            text.push_str(&format!("TEL:{phone}\r\n"));
        }
        text.push_str("END:VCARD\r\n");
        VCard::parse(&text).unwrap()
    }

    #[test]
    fn shared_emails_group_case_insensitively() {
        let a = card("a", "Ada Lovelace", Some("Ada@Example.com"), None);
        let b = card("b", "Ada L.", Some("ada@example.com"), None);
        let c = card("c", "Grace Hopper", Some("grace@example.com"), None);
        let refs = [&a, &b, &c];
        let groups = duplicate_groups(&refs);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].indices, [0, 1]);
        assert!(groups[0].reasons.iter().any(|reason| reason.kind == MatchKind::Email));
    }

    #[test]
    fn phones_group_across_local_and_international_formatting() {
        let a = card("a", "Ada Lovelace", None, Some("020 7946 0018"));
        let b = card("b", "Ada L.", None, Some("+44 20 7946 0018"));
        let refs = [&a, &b];
        let groups = duplicate_groups(&refs);
        assert_eq!(groups.len(), 1, "the trailing-nine-digit key matches the country-code form");
    }

    #[test]
    fn bare_contacts_with_the_same_name_group_but_complete_ones_do_not() {
        let bare_a = card("a", "Jane Doe", None, None);
        let bare_b = card("b", "Doe, Jane", None, None);
        // A same-name pair with real contact details: a shared name alone must
        // not glue these two together, and the bare pair must still group
        // without them.
        let complete_a = card("c", "Jane Doe", Some("jane@one.example"), None);
        let complete_b = card("d", "Jane Doe", Some("jane@two.example"), None);
        let refs = [&bare_a, &bare_b, &complete_a, &complete_b];
        let groups = duplicate_groups(&refs);
        assert_eq!(groups.len(), 1, "only the two bare cards share a name-only edge");
        assert_eq!(groups[0].indices, [0, 1]);
        assert!(groups[0].reasons.iter().any(|reason| reason.kind == MatchKind::Name));
    }

    #[test]
    fn contact_lists_are_ignored_even_when_they_share_an_address() {
        let person = card("a", "Ada Lovelace", Some("ada@example.com"), None);
        let list = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nKIND:group\r\nUID:list\r\nFN:Crew\r\nEMAIL:ada@example.com\r\nEND:VCARD\r\n").unwrap();
        let refs = [&person, &list];
        assert!(duplicate_groups(&refs).is_empty());
    }

    #[test]
    fn merge_fields_deduplicate_values_and_keep_sources() {
        let a = VCard::parse(
            "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:a\r\nFN:Ada Lovelace\r\nEMAIL;TYPE=work:ada@example.com\r\nTEL:+44 20 7946 0018\r\nCATEGORIES:Friends\r\nEND:VCARD\r\n",
        )
        .unwrap();
        let b = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:b\r\nFN:Ada Lovelace\r\nEMAIL;TYPE=home:ADA@example.com\r\nTEL:020 7946 0018\r\nCATEGORIES:Work\r\nEND:VCARD\r\n")
            .unwrap();
        let refs = [&a, &b];
        let fields = MergeFields::build(&refs);
        let emails = &fields.lists[&MergeList::Email];
        assert_eq!(emails.len(), 1, "case variants are one candidate");
        assert_eq!(emails[0].sources, [0, 1]);
        let categories = &fields.lists[&MergeList::Category];
        assert_eq!(categories.len(), 2);
        let names = &fields.scalars[&MergeScalar::Name];
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].sources, [0, 1]);
    }

    #[test]
    fn default_choices_prefer_the_keeper_and_keep_every_list_value() {
        let a = card("a", "Ada Lovelace", Some("ada@example.com"), None);
        let b = card("b", "Ada King", Some("ada.king@example.com"), Some("020 7946 0018"));
        let refs = [&a, &b];
        let choices = default_choices(&refs, 0);
        assert_eq!(choices.scalars[&MergeScalar::Name], Some(0));
        assert_eq!(choices.scalars.get(&MergeScalar::Organization), None);
        assert_eq!(choices.lists[&MergeList::Email].len(), 2);
        assert!(choices.combine_notes);
    }

    #[test]
    fn merge_cards_honours_field_choices_and_unions_lists() {
        let a = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:a\r\nFN:Ada Lovelace\r\nORG:Analytical Engines\r\nEMAIL;TYPE=work:ada@example.com\r\nTEL;TYPE=work:+44 20 7946 0018\r\nCATEGORIES:Friends\r\nEND:VCARD\r\n").unwrap();
        let b = VCard::parse(
            "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:b\r\nFN:Ada King\r\nEMAIL;TYPE=home:ada.king@example.com\r\nTEL;TYPE=home:020 7946 0018\r\nCATEGORIES:Work\r\nEND:VCARD\r\n",
        )
        .unwrap();
        let refs = [&a, &b];
        let mut choices = MergeChoices::default();
        choices.scalars.insert(MergeScalar::Name, Some(1));
        choices.scalars.insert(MergeScalar::Organization, Some(0));
        choices
            .lists
            .insert(MergeList::Email, ["ada@example.com".to_string(), "ada.king@example.com".to_string()].into());
        choices.lists.insert(MergeList::Phone, ["2079460018".to_string()].into());
        choices.lists.insert(MergeList::Category, ["friends".to_string(), "work".to_string()].into());
        let merged = merge_cards(&refs, 0, &choices).unwrap();
        assert_eq!(merged.full_name.as_deref(), Some("Ada King"));
        assert_eq!(merged.organization.as_ref().unwrap()[0], "Analytical Engines");
        assert_eq!(merged.emails.len(), 2);
        assert_eq!(merged.telephones.len(), 1);
        assert_eq!(merged.categories, ["Friends", "Work"]);
        assert_eq!(merged.uid.as_deref(), Some("a"), "the keeper's server identity is preserved");
    }

    #[test]
    fn merge_cards_combines_notes_and_merges_phone_types() {
        let a = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:a\r\nFN:Ada\r\nTEL;TYPE=work:+44 20 7946 0018\r\nNOTE:First note\r\nEND:VCARD\r\n").unwrap();
        let b = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:b\r\nFN:Ada\r\nTEL;TYPE=home:+44 20 7946 0018\r\nNOTE:Second note\r\nEND:VCARD\r\n").unwrap();
        let refs = [&a, &b];
        let mut choices = MergeChoices {
            combine_notes: true,
            ..MergeChoices::default()
        };
        choices.lists.insert(MergeList::Phone, ["442079460018".to_string()].into());
        let merged = merge_cards(&refs, 0, &choices).unwrap();
        assert_eq!(merged.note.as_deref(), Some("First note\n\nSecond note"));
        assert_eq!(merged.telephones.len(), 1);
        assert_eq!(merged.telephones[0].types, ["work", "home"]);
    }

    #[test]
    fn merged_cards_round_trip_through_serialization() {
        let a = card("a", "Ada Lovelace", Some("ada@example.com"), None);
        let b = card("b", "Ada King", Some("ada.king@example.com"), Some("+44 20 7946 0018"));
        let refs = [&a, &b];
        let choices = default_choices(&refs, 0);
        let merged = merge_cards(&refs, 0, &choices).unwrap();
        let text = merged.serialize();
        let reparsed = VCard::parse(&text).unwrap();
        assert_eq!(reparsed.full_name.as_deref(), Some("Ada Lovelace"));
        assert_eq!(reparsed.emails.len(), 2);
        assert_eq!(reparsed.telephones.len(), 1);
    }

    #[test]
    fn completeness_prefers_a_filled_card() {
        let sparse = card("a", "Ada", None, None);
        let full = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nUID:b\r\nFN:Ada\r\nORG:Engines\r\nEMAIL:ada@example.com\r\nTEL:+15551234\r\nNOTE:Hi\r\nEND:VCARD\r\n").unwrap();
        assert!(completeness(&full) > completeness(&sparse));
    }
}
