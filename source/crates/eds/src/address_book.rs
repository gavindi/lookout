/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lookout_core::vcard::VCard;
use zbus::proxy::CacheProperties;

use crate::proxies::{AddressBookFactoryProxy, AddressBookProxy};
use crate::{Eds, Result};

/// One opened EDS address book. Holds the proxy for the backend object the
/// factory handed back, so repeated searches skip the factory round trip.
///
/// EDS keeps the backend open for as long as the connection that opened it
/// stays on the bus; [`Eds`] shares one connection, so call
/// [`close`](Self::close) when the book is no longer needed.
pub struct EdsAddressBook {
    source_uid: String,
    proxy: AddressBookProxy<'static>,
}

impl EdsAddressBook {
    /// Opens the book for `source_uid` through the address-book factory, then
    /// calls `Open` on the backend, which EDS requires before any query.
    pub async fn open(eds: &Eds, source_uid: &str) -> Result<EdsAddressBook> {
        let factory = AddressBookFactoryProxy::builder(eds.connection()).cache_properties(CacheProperties::No).build().await?;
        // The backend may live on the factory's own bus name or on a
        // per-backend subprocess name, so use the pair exactly as returned.
        let (object_path, bus_name) = factory.open_address_book(source_uid).await?;
        let proxy = AddressBookProxy::builder(eds.connection())
            .destination(bus_name)?
            .path(object_path)?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        proxy.open().await?;
        Ok(EdsAddressBook {
            source_uid: source_uid.to_string(),
            proxy,
        })
    }

    pub fn source_uid(&self) -> &str {
        &self.source_uid
    }

    /// Contacts with at least one email address whose name, nickname or an
    /// email address starts with `query`, at most `limit` of them, sorted by
    /// name. Contact lists are left out, and a card that doesn't parse is
    /// skipped. A blank query returns nothing without asking EDS.
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<VCard>> {
        let Some(sexp) = search_query(query) else {
            return Ok(Vec::new());
        };
        let vcards = self.proxy.get_contact_list(&sexp).await?;
        Ok(cards_from_vcards(&vcards, limit, &self.source_uid))
    }

    /// Every card in the book, contact lists included, in EDS's order. A card
    /// that doesn't parse is skipped. For browsing a whole book (the People
    /// screen), where [`search`](Self::search) is for recipient lookup.
    pub async fn all_contacts(&self) -> Result<Vec<VCard>> {
        // EDS's SQLite-backed books (local, and the caches of remote ones)
        // reject `#t`; an empty any-field match is their "everything". Fall
        // back to `#t` for a backend that doesn't know the any-field form.
        let vcards = match self.proxy.get_contact_list(MATCH_ALL).await {
            Ok(vcards) => vcards,
            Err(e) => {
                tracing::debug!("EDS: {MATCH_ALL} failed on {} ({e}); retrying with #t", self.source_uid);
                self.proxy.get_contact_list("#t").await?
            }
        };
        Ok(parse_vcards(&vcards, &self.source_uid))
    }

    /// Releases the backend. Errors are ignored: a book that can't be closed
    /// is closed anyway when the connection goes.
    pub async fn close(self) {
        if let Err(e) = self.proxy.close().await {
            tracing::debug!("EDS: closing address book {}: {e}", self.source_uid);
        }
    }
}

/// The EBook S-expression matching every card in a SQLite-backed book.
const MATCH_ALL: &str = r#"(contains "x-evolution-any-field" "")"#;

/// The EBook S-expression for a prefix search on `query` among contacts that
/// have an email address, or `None` when the trimmed query is empty.
///
/// Cards without an address are filtered out by EDS rather than here, so they
/// never use up the caller's `limit` (address books hold plenty of phone-only
/// contacts).
///
/// Field names use underscores (`full_name`; a hyphenated `full-name`
/// silently matches nothing). On EDS's own SQLite-backed books `full_name`
/// already matches the full name, family name, given name or nickname, but
/// `nickname` is asked for too, for backends that translate the expression
/// themselves (LDAP). EDS folds case and accents, so the query isn't
/// lower-cased. Inside the string literal `\` and `"` are escaped.
pub fn search_query(query: &str) -> Option<String> {
    let query = query.trim();
    if query.is_empty() {
        return None;
    }
    let literal = escape(query);
    Some(format!(
        r#"(and (exists "email") (or (beginswith "full_name" "{literal}") (beginswith "nickname" "{literal}") (beginswith "email" "{literal}")))"#
    ))
}

fn escape(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '"') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Parses the backend's vCards, drops contact lists and unparseable cards,
/// sorts by name and keeps the first `limit`.
fn cards_from_vcards(vcards: &[String], limit: usize, source_uid: &str) -> Vec<VCard> {
    let mut cards: Vec<VCard> = parse_vcards(vcards, source_uid).into_iter().filter(|card| !is_list(card)).collect();
    cards.sort_by_cached_key(sort_key);
    cards.truncate(limit);
    cards
}

/// Parses the backend's vCards, skipping (and logging) any that don't parse.
fn parse_vcards(vcards: &[String], source_uid: &str) -> Vec<VCard> {
    vcards
        .iter()
        .filter_map(|text| match VCard::parse(text) {
            Ok(card) => Some(card),
            Err(e) => {
                tracing::debug!("EDS: skipping an unparseable card from {source_uid}: {e}");
                None
            }
        })
        .collect()
}

/// Evolution marks contact lists with `X-EVOLUTION-LIST:TRUE` (their `EMAIL`
/// lines are the members), other clients with `KIND:group`.
fn is_list(card: &VCard) -> bool {
    card.is_contact_list()
        || card
            .other
            .iter()
            .any(|p| p.name.eq_ignore_ascii_case("X-EVOLUTION-LIST") && p.value.trim().eq_ignore_ascii_case("true"))
}

fn sort_key(card: &VCard) -> String {
    display_name(card)
        .or_else(|| card.emails.first().map(|e| e.address.clone()))
        .unwrap_or_default()
        .to_lowercase()
}

/// The card's full name, or one built from its structured name. Some books
/// (Microsoft 365's, through GOA) hand out cards with only an address.
fn display_name(card: &VCard) -> Option<String> {
    let full_name = card.full_name.as_deref().map(str::trim).filter(|n| !n.is_empty());
    full_name.map(str::to_string).or_else(|| {
        let name = card.name.as_ref()?;
        let parts: Vec<&str> = [&name.prefix, &name.given, &name.family, &name.suffix]
            .into_iter()
            .map(|p| p.trim())
            .filter(|p| !p.is_empty())
            .collect();
        (!parts.is_empty()).then(|| parts.join(" "))
    })
}

/// One address from an EDS contact, as a recipient suggestion needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdsPerson {
    /// `None` when the card has neither a full nor a structured name.
    pub name: Option<String>,
    pub address: String,
    /// The first `ORG` component.
    pub organization: Option<String>,
    pub title: Option<String>,
}

/// One [`EdsPerson`] per email address on `card`, in the card's order, with
/// blank addresses skipped. A contact list yields nothing.
pub fn people_from_card(card: &VCard) -> Vec<EdsPerson> {
    if is_list(card) {
        return Vec::new();
    }
    let name = display_name(card);
    let organization = card
        .organization
        .as_ref()
        .and_then(|org| org.first())
        .map(|o| o.trim().to_string())
        .filter(|o| !o.is_empty());
    let title = card.title.as_deref().map(str::trim).filter(|t| !t.is_empty()).map(str::to_string);
    card.emails
        .iter()
        .map(|email| email.address.trim())
        .filter(|address| !address.is_empty())
        .map(|address| EdsPerson {
            name: name.clone(),
            address: address.to_string(),
            organization: organization.clone(),
            title: title.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_searches_name_nickname_and_email_prefixes() {
        assert_eq!(
            search_query("ann").unwrap(),
            r#"(and (exists "email") (or (beginswith "full_name" "ann") (beginswith "nickname" "ann") (beginswith "email" "ann")))"#
        );
    }

    #[test]
    fn query_is_trimmed_but_keeps_its_case() {
        assert_eq!(search_query("  Ann Ex \t").unwrap(), search_query("Ann Ex").unwrap());
        assert!(search_query("Ann").unwrap().contains(r#""Ann""#));
    }

    #[test]
    fn blank_query_builds_nothing() {
        assert_eq!(search_query(""), None);
        assert_eq!(search_query("  \t\n"), None);
    }

    #[test]
    fn quotes_and_backslashes_are_escaped() {
        assert_eq!(
            search_query(r#"a"b\c"#).unwrap(),
            r#"(and (exists "email") (or (beginswith "full_name" "a\"b\\c") (beginswith "nickname" "a\"b\\c") (beginswith "email" "a\"b\\c")))"#
        );
        // A trailing backslash can't escape the closing quote.
        assert!(search_query(r"ann\").unwrap().contains(r#""ann\\"))"#));
    }

    #[test]
    fn non_ascii_passes_through_unchanged() {
        assert!(search_query("Émile").unwrap().contains(r#"(beginswith "full_name" "Émile")"#));
        assert!(search_query("李").unwrap().contains(r#"(beginswith "email" "李")"#));
    }

    const ANN: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Ann Example\r\nN:Example;Ann;;;\r\nNICKNAME:Annie\r\n\
        EMAIL;TYPE=WORK:ann@work.example\r\nEMAIL;TYPE=HOME:Ann.Home@example.org\r\nEMAIL: \r\n\
        ORG:Example Pty Ltd;R&D\r\nTITLE:Engineer\r\nUID:pas-id-1\r\nEND:VCARD\r\n";

    #[test]
    fn a_card_with_several_addresses_gives_one_person_per_address() {
        let card = VCard::parse(ANN).unwrap();
        let person = |address: &str| EdsPerson {
            name: Some("Ann Example".to_string()),
            address: address.to_string(),
            organization: Some("Example Pty Ltd".to_string()),
            title: Some("Engineer".to_string()),
        };
        assert_eq!(people_from_card(&card), [person("ann@work.example"), person("Ann.Home@example.org")]);
    }

    #[test]
    fn a_card_without_names_gives_nameless_people() {
        // The shape Microsoft 365's GOA book returns.
        let card = VCard::parse("BEGIN:VCARD\r\nVERSION:3.0\r\nUID:x\r\nEMAIL:bob@example.com\r\nEND:VCARD\r\n").unwrap();
        assert_eq!(
            people_from_card(&card),
            [EdsPerson {
                name: None,
                address: "bob@example.com".to_string(),
                organization: None,
                title: None,
            }]
        );
    }

    #[test]
    fn a_structured_name_stands_in_for_a_missing_full_name() {
        let card = VCard::parse("BEGIN:VCARD\r\nVERSION:3.0\r\nN:Zola;Émile;;;\r\nEMAIL:emile@example.fr\r\nEND:VCARD\r\n").unwrap();
        assert_eq!(people_from_card(&card)[0].name.as_deref(), Some("Émile Zola"));
    }

    const EVOLUTION_LIST: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:Team\r\nX-EVOLUTION-LIST:TRUE\r\n\
        EMAIL;X-EVOLUTION-DEST-CONTACT-UID=pas-id-1:Ann <ann@work.example>\r\nEND:VCARD\r\n";

    #[test]
    fn contact_lists_give_no_people() {
        assert!(people_from_card(&VCard::parse(EVOLUTION_LIST).unwrap()).is_empty());
        let group = VCard::parse("BEGIN:VCARD\r\nVERSION:4.0\r\nKIND:group\r\nFN:Group\r\nEMAIL:g@example.com\r\nEND:VCARD\r\n").unwrap();
        assert!(people_from_card(&group).is_empty());
    }

    #[test]
    fn results_drop_lists_and_bad_cards_then_sort_and_limit() {
        let vcards = [
            "BEGIN:VCARD\r\nVERSION:3.0\r\nFN:carol\r\nEMAIL:c@example.com\r\nEND:VCARD\r\n".to_string(),
            EVOLUTION_LIST.to_string(),
            "not a vcard".to_string(),
            "BEGIN:VCARD\r\nVERSION:3.0\r\nEMAIL:aaron@example.com\r\nEND:VCARD\r\n".to_string(),
            ANN.to_string(),
        ];
        let names = |cards: Vec<VCard>| cards.iter().map(sort_key).collect::<Vec<_>>();
        assert_eq!(names(cards_from_vcards(&vcards, 10, "book")), ["aaron@example.com", "ann example", "carol"]);
        assert_eq!(names(cards_from_vcards(&vcards, 2, "book")), ["aaron@example.com", "ann example"]);
        assert!(cards_from_vcards(&vcards, 0, "book").is_empty());
    }
}
