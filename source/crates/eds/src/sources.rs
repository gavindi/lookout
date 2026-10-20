/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::{HashMap, HashSet};

use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::keyfile::{language_names, KeyFile};
use crate::proxies::{SourceObjectManagerProxy, SOURCE_INTERFACE};
use crate::{Eds, Result};

/// What an [`EdsSource`] holds. Sources of other kinds (collections, mail
/// identities and transports, memo lists, signatures, the `*-stub` parents)
/// aren't listed, though collections still supply `goa_account_id`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum EdsSourceKind {
    AddressBook,
    Calendar,
    TaskList,
    MailAccount,
}

impl EdsSourceKind {
    /// The key-file group that marks the kind and holds its `BackendName`.
    fn group(self) -> &'static str {
        match self {
            EdsSourceKind::AddressBook => "Address Book",
            EdsSourceKind::Calendar => "Calendar",
            EdsSourceKind::TaskList => "Task List",
            EdsSourceKind::MailAccount => "Mail Account",
        }
    }

    const ALL: [EdsSourceKind; 4] = [EdsSourceKind::AddressBook, EdsSourceKind::Calendar, EdsSourceKind::TaskList, EdsSourceKind::MailAccount];
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdsSource {
    pub uid: String,
    /// Translated for the user's locale when the source carries translations
    /// (the built-in "Personal" book and calendar do).
    pub display_name: String,
    pub kind: EdsSourceKind,
    /// `None` for top-level sources.
    pub parent_uid: Option<String>,
    /// Whether the source and every ancestor are enabled - the same rule GNOME
    /// Shell's calendar uses. A book inside a disabled GOA collection is off
    /// even though its own `Enabled` key says true.
    pub enabled: bool,
    /// `BackendName` (`local`, `carddav`, `caldav`, `webcal`, `imapx`, ...).
    pub backend: Option<String>,
    /// The GOA account id (`account_…`) of the nearest ancestor (or the
    /// source itself) with a `[GNOME Online Accounts]` group - in practice
    /// the source's collection. Lets callers recognise sources Lookout
    /// already syncs through GOA.
    pub goa_account_id: Option<String>,
}

impl Eds {
    /// Lists every address book, calendar, task list and mail account the
    /// registry knows, sorted by kind, then display name, then UID.
    pub async fn sources(&self) -> Result<Vec<EdsSource>> {
        let raw = managed_sources(self).await?;
        Ok(parse_sources(raw.iter().map(|s| (s.uid.as_str(), s.data.as_str())), &language_names()))
    }
}

/// One source object as the registry lists it, before its key file is parsed.
/// The object path is needed by callers that write to or remove a source;
/// `UID` is the stable key, since `Source_N` paths are renumbered.
pub(crate) struct ManagedSource {
    pub path: OwnedObjectPath,
    pub uid: String,
    pub data: String,
}

/// Reads `GetManagedObjects` and keeps the source objects (every one with a
/// `UID` and `Data`).
pub(crate) async fn managed_sources(eds: &Eds) -> Result<Vec<ManagedSource>> {
    let manager = SourceObjectManagerProxy::new(eds.connection()).await?;
    let objects = manager.get_managed_objects().await?;
    Ok(objects
        .into_iter()
        .filter_map(|(path, interfaces)| {
            let props = interfaces.get(SOURCE_INTERFACE)?;
            Some(ManagedSource {
                path,
                uid: get_string(props, "UID")?,
                data: get_string(props, "Data")?,
            })
        })
        .collect())
}

fn get_string(props: &HashMap<String, OwnedValue>, key: &str) -> Option<String> {
    props.get(key).and_then(|v| String::try_from(v.clone()).ok())
}

/// Builds the source list from each source's `(UID, Data)`. A source whose
/// key file doesn't parse is skipped (and logged); it can still be another
/// source's parent only in the sense that its children then find no GOA id
/// or `Enabled` key there.
fn parse_sources<'a>(raw: impl IntoIterator<Item = (&'a str, &'a str)>, locales: &[String]) -> Vec<EdsSource> {
    let mut files: HashMap<&str, KeyFile> = HashMap::new();
    for (uid, data) in raw {
        match KeyFile::parse(data) {
            Ok(file) => {
                files.insert(uid, file);
            }
            Err(e) => tracing::warn!("EDS: skipping source {uid}: {e}"),
        }
    }

    let mut sources: Vec<EdsSource> = files
        .iter()
        .filter_map(|(&uid, file)| {
            let kind = EdsSourceKind::ALL.into_iter().find(|kind| file.has_group(kind.group()))?;
            let ancestry = ancestry(uid, &files);
            Some(EdsSource {
                uid: uid.to_string(),
                display_name: file.get_locale_string("Data Source", "DisplayName", locales).unwrap_or_default().to_string(),
                kind,
                parent_uid: parent_of(file).map(str::to_string),
                // ESource defaults `Enabled` to true when the key is missing.
                enabled: ancestry.iter().all(|f| f.get_bool("Data Source", "Enabled").unwrap_or(true)),
                backend: file.get_non_empty(kind.group(), "BackendName").map(str::to_string),
                goa_account_id: ancestry.iter().find_map(|f| f.get_non_empty("GNOME Online Accounts", "AccountId")).map(str::to_string),
            })
        })
        .collect();
    sources.sort_by(|a, b| (a.kind as u8, a.display_name.to_lowercase(), &a.uid).cmp(&(b.kind as u8, b.display_name.to_lowercase(), &b.uid)));
    sources
}

fn parent_of(file: &KeyFile) -> Option<&str> {
    file.get_non_empty("Data Source", "Parent")
}

/// The source's key file followed by each ancestor's, nearest first. Stops at
/// a parent the registry doesn't list, and at a cycle, which a hand-edited
/// `.source` file could create.
fn ancestry<'f>(uid: &str, files: &'f HashMap<&str, KeyFile>) -> Vec<&'f KeyFile> {
    let mut chain = Vec::new();
    let mut seen = HashSet::new();
    let mut next = Some(uid);
    while let Some(current) = next {
        if !seen.insert(current) {
            tracing::warn!("EDS: source {uid} has a parent cycle through {current}");
            break;
        }
        let Some(file) = files.get(current) else { break };
        chain.push(file);
        next = parent_of(file);
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    fn source(display_name: &str, parent: &str, body: &str) -> String {
        format!("[Data Source]\nDisplayName={display_name}\nEnabled=true\nParent={parent}\n\n{body}")
    }

    fn fixture() -> Vec<(&'static str, String)> {
        vec![
            ("local-stub", source("On This Computer", "", "")),
            (
                "system-address-book",
                "[Data Source]\nDisplayName=Personal\nDisplayName[de]=Persönlich\nEnabled=true\nParent=local-stub\n\n[Address Book]\nBackendName=local\n".to_string(),
            ),
            (
                "system-calendar",
                source("Personal", "local-stub", "[Calendar]\nBackendName=local\nColor=#f5c211\nSelected=true\n"),
            ),
            (
                "goa-collection",
                source(
                    "me@example.com",
                    "",
                    "[Collection]\nBackendName=google\n\n[GNOME Online Accounts]\nAccountId=account_1676261544_5\n",
                ),
            ),
            ("goa-book", source("Contacts", "goa-collection", "[Address Book]\nBackendName=carddav\n")),
            (
                "goa-calendar",
                source("me@example.com", "goa-collection", "[Calendar]\nBackendName=caldav\nSelected=true\n"),
            ),
            ("goa-tasks", source("My Tasks", "goa-collection", "[Task List]\nBackendName=gtasks\n")),
            ("goa-mail", source("me@example.com", "goa-collection", "[Mail Account]\nBackendName=imapx\n")),
            ("goa-identity", source("me@example.com", "goa-collection", "[Mail Identity]\nAddress=me@example.com\n")),
            ("goa-transport", source("me@example.com", "goa-collection", "[Mail Transport]\nBackendName=smtp\n")),
            ("imap-account", source("work@example.org", "", "[Mail Account]\nBackendName=imapx\n")),
            ("memos", source("Memos", "local-stub", "[Memo List]\nBackendName=local\n")),
            (
                "off-collection",
                "[Data Source]\nDisplayName=Old\nEnabled=false\n\n[Collection]\nBackendName=webdav\n\n[GNOME Online Accounts]\nAccountId=account_2\n".to_string(),
            ),
            ("off-book", source("Old contacts", "off-collection", "[Address Book]\nBackendName=carddav\n")),
            (
                "ldap-book",
                "[Data Source]\nDisplayName=Directory\nParent=ldap-stub\n\n[Address Book]\nBackendName=ldap\n".to_string(),
            ),
            ("broken", "[Data Source\nDisplayName=Broken\n".to_string()),
        ]
    }

    fn parse(locales: &[&str]) -> Vec<EdsSource> {
        let raw = fixture();
        let locales: Vec<String> = locales.iter().map(|l| l.to_string()).collect();
        parse_sources(raw.iter().map(|(uid, data)| (*uid, data.as_str())), &locales)
    }

    fn find<'a>(sources: &'a [EdsSource], uid: &str) -> &'a EdsSource {
        sources.iter().find(|s| s.uid == uid).unwrap_or_else(|| panic!("{uid} missing from {sources:#?}"))
    }

    #[test]
    fn lists_only_the_four_kinds_in_order() {
        let sources = parse(&[]);
        let uids: Vec<&str> = sources.iter().map(|s| s.uid.as_str()).collect();
        assert_eq!(
            uids,
            [
                // Address books, then calendars, task lists and mail accounts,
                // each by display name.
                "goa-book",
                "ldap-book",
                "off-book",
                "system-address-book",
                "goa-calendar",
                "system-calendar",
                "goa-tasks",
                "goa-mail",
                "imap-account",
            ]
        );
    }

    #[test]
    fn reads_kind_backend_and_parent() {
        let sources = parse(&[]);
        let book = find(&sources, "system-address-book");
        assert_eq!(book.kind, EdsSourceKind::AddressBook);
        assert_eq!(book.display_name, "Personal");
        assert_eq!(book.backend.as_deref(), Some("local"));
        assert_eq!(book.parent_uid.as_deref(), Some("local-stub"));
        assert!(book.enabled);
        assert_eq!(book.goa_account_id, None);

        let account = find(&sources, "imap-account");
        assert_eq!(account.kind, EdsSourceKind::MailAccount);
        assert_eq!(account.parent_uid, None, "an empty Parent= is no parent");
        assert_eq!(find(&sources, "goa-tasks").kind, EdsSourceKind::TaskList);
        assert_eq!(find(&sources, "goa-calendar").backend.as_deref(), Some("caldav"));
    }

    #[test]
    fn goa_account_id_comes_from_the_collection_parent() {
        let sources = parse(&[]);
        for uid in ["goa-book", "goa-calendar", "goa-tasks", "goa-mail"] {
            assert_eq!(find(&sources, uid).goa_account_id.as_deref(), Some("account_1676261544_5"), "{uid}");
        }
        assert_eq!(find(&sources, "off-book").goa_account_id.as_deref(), Some("account_2"));
        assert_eq!(find(&sources, "ldap-book").goa_account_id, None);
    }

    #[test]
    fn a_disabled_ancestor_disables_its_children() {
        let sources = parse(&[]);
        assert!(!find(&sources, "off-book").enabled);
        assert!(find(&sources, "goa-book").enabled);
    }

    #[test]
    fn missing_enabled_key_and_unlisted_parent_mean_enabled() {
        // `ldap-book` has no Enabled key and its parent isn't in the registry.
        let sources = parse(&[]);
        let book = find(&sources, "ldap-book");
        assert!(book.enabled);
        assert_eq!(book.parent_uid.as_deref(), Some("ldap-stub"));
    }

    #[test]
    fn display_names_are_translated() {
        assert_eq!(find(&parse(&["de_DE", "de"]), "system-address-book").display_name, "Persönlich");
        assert_eq!(find(&parse(&["fr_FR", "fr"]), "system-address-book").display_name, "Personal");
    }

    #[test]
    fn malformed_sources_are_skipped() {
        assert!(parse(&[]).iter().all(|s| s.uid != "broken"));
    }

    #[test]
    fn parent_cycles_terminate() {
        let raw = [
            ("a", source("A", "b", "[Address Book]\nBackendName=local\n\n[GNOME Online Accounts]\nAccountId=\n")),
            ("b", source("B", "a", "[Collection]\n")),
            ("self", source("Self", "self", "[Calendar]\nBackendName=local\n")),
        ];
        let sources = parse_sources(raw.iter().map(|(uid, data)| (*uid, data.as_str())), &[]);
        assert_eq!(sources.len(), 2);
        assert!(sources.iter().all(|s| s.enabled && s.goa_account_id.is_none()));
    }
}
