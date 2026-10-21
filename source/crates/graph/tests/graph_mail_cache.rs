//! The Graph mail transport's cache strategy (MAIL_GRAPH_OVERVIEW.md item
//! 5.7.4): Graph folders and messages, mapped by `lookout_graph` (item 5.7.2)
//! under the identity model (item 5.7.3), are stored in the same per-account
//! `lookout_mail::Cache` an IMAP account uses. These tests drive the synthetic
//! Graph fixtures through that path and check that the cache's threading,
//! search, overlays, moves and bodies all work on Graph data unchanged.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use chrono::{Duration, Utc};
use lookout_core::{tag_keyword, AccountId, Importance, MailboxId, MailboxRole, SystemFlagBit, ThreadKey, Uid};
use lookout_graph::{
    allocation_order, attachment_from_graph, email_body_from_graph, email_summary_from_graph, graph_mailbox_id, mail_folder_from_graph, mailbox_from_graph, message_from_graph,
    GraphMessage, WellKnownFolders,
};
use lookout_mail::{remove_account_cache, Cache, RemoteMailProperties};
use serde_json::Value;

const INBOX: &str = "AAMkAGINBOX=";
const PROJECTS: &str = "AAMkAGPROJECTS=";

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn delta_messages() -> Vec<GraphMessage> {
    let page = fixture(include_str!("../../../test-fixtures/outlook/message-summary-delta.json"));
    page["value"].as_array().unwrap().iter().filter_map(|item| message_from_graph(item, None)).collect()
}

/// The fixture's Inbox messages stored the way a Graph session would: new
/// Uids handed out oldest-first, then one upsert for the folder.
fn store_inbox(cache: &Cache, account: &AccountId) -> Vec<(GraphMessage, Uid)> {
    let mut messages: Vec<GraphMessage> = delta_messages().into_iter().filter(|m| m.folder_id.as_deref() == Some(INBOX)).collect();
    allocation_order(&mut messages);
    let ids: Vec<&str> = messages.iter().map(|m| m.graph_id.as_str()).collect();
    let uids = cache.graph_uids(&ids).unwrap();
    let inbox = graph_mailbox_id(account, INBOX);
    let summaries: Vec<_> = messages.iter().zip(&uids).map(|(m, uid)| email_summary_from_graph(m, inbox.clone(), *uid)).collect();
    cache.upsert_messages(&inbox, cache.graph_uid_validity().unwrap(), &summaries).unwrap();
    messages.into_iter().zip(uids).collect()
}

/// One cached summary with the local overlays applied, as the app reads it
/// (`load_summary` returns the raw stored row).
fn overlaid(cache: &Cache, mailbox: &MailboxId, uid: Uid) -> lookout_core::EmailSummary {
    cache.load_summaries(mailbox, &[uid]).unwrap().remove(&uid).unwrap()
}

fn uid_of(stored: &[(GraphMessage, Uid)], graph_id: &str) -> Uid {
    stored.iter().find(|(m, _)| m.graph_id == graph_id).unwrap().1
}

struct TempAccount(AccountId);

impl Drop for TempAccount {
    fn drop(&mut self) {
        let _ = remove_account_cache(&self.0);
    }
}

fn temp_account() -> TempAccount {
    TempAccount(AccountId(format!("/test/graph_mail_cache_{}", uuid_like())))
}

fn uuid_like() -> String {
    format!("{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')'], "")
}

#[test]
fn graph_folders_are_stored_as_the_mailbox_list() {
    let account = temp_account();
    let cache = Cache::open(&account.0).unwrap();
    let page = fixture(include_str!("../../../test-fixtures/outlook/mail-folders-delta.json"));
    let folders: Vec<_> = page["value"].as_array().unwrap().iter().filter_map(mail_folder_from_graph).collect();
    let mut well_known = WellKnownFolders::default();
    well_known.set("inbox", Some(INBOX.to_string()));
    let validity = cache.graph_uid_validity().unwrap();
    let mailboxes: Vec<_> = folders
        .iter()
        .map(|folder| {
            let parent = folder
                .parent_id
                .as_deref()
                .filter(|parent| folders.iter().any(|f| f.id == *parent))
                .map(|parent| graph_mailbox_id(&account.0, parent));
            mailbox_from_graph(folder, &account.0, graph_mailbox_id(&account.0, &folder.id), parent, well_known.role_of(folder), validity)
        })
        .collect();
    cache.replace_mailboxes(&account.0, &mailboxes).unwrap();

    let loaded = cache.load_mailboxes(&account.0).unwrap();
    assert_eq!(loaded.len(), 4);
    let projects = loaded.iter().find(|m| m.name == "Projects").unwrap();
    assert_eq!(projects.parent, Some(graph_mailbox_id(&account.0, INBOX)));
    assert!(loaded.iter().all(|m| m.uidvalidity == validity), "one validity for every folder");
    assert_eq!(loaded.iter().find(|m| m.name == "Inbox").unwrap().role, MailboxRole::Inbox);
    assert_eq!(
        loaded.iter().find(|m| m.name == "Deleted Items").unwrap().role,
        MailboxRole::Trash,
        "the name guess still applies"
    );
}

#[test]
fn graph_messages_keep_their_threads_flags_and_search() {
    let account = temp_account();
    let cache = Cache::open(&account.0).unwrap();
    let stored = store_inbox(&cache, &account.0);
    let inbox = graph_mailbox_id(&account.0, INBOX);

    let loaded = cache.load_messages(&inbox).unwrap();
    assert_eq!(loaded.len(), 3);
    let report = loaded.iter().find(|m| m.uid == uid_of(&stored, "AAMkAGMSG1=")).unwrap();
    let reply = loaded.iter().find(|m| m.uid == uid_of(&stored, "AAMkAGMSG2=")).unwrap();
    // The cache stores the conversation key as given; nothing recomputes it.
    assert_eq!(report.thread_key, ThreadKey("graph:AAQkAGCONV1=".to_string()));
    assert_eq!(reply.thread_key, report.thread_key);
    assert!(report.is_unread());
    assert!(report.is_flagged_for_follow_up());
    assert!(reply.flags.contains(&SystemFlagBit::Seen));
    assert!(loaded.iter().any(|m| m.has_calendar), "the meeting request");
    // Oldest first: the report arrived before the reply.
    assert!(report.uid < reply.uid);

    let found = cache.search("quarterly", 10).unwrap();
    assert_eq!(found.len(), 2, "subject and preview are indexed");

    // Storing the same delta again changes nothing and keeps every Uid.
    let again = store_inbox(&cache, &account.0);
    assert_eq!(
        again.iter().map(|(_, uid)| *uid).collect::<Vec<_>>(),
        stored.iter().map(|(_, uid)| *uid).collect::<Vec<_>>()
    );
    assert_eq!(cache.load_messages(&inbox).unwrap().len(), 3);
}

#[test]
fn graph_properties_flow_through_the_existing_overlays() {
    let account = temp_account();
    let cache = Cache::open(&account.0).unwrap();
    let stored = store_inbox(&cache, &account.0);
    let inbox = graph_mailbox_id(&account.0, INBOX);
    let reply_uid = uid_of(&stored, "AAMkAGMSG2=");
    let reply = overlaid(&cache, &inbox, reply_uid);

    // A stale local overlay (say, from when this account synced over IMAP).
    cache
        .set_local_importance(std::slice::from_ref(&reply), Importance::High, Utc::now() - Duration::hours(1))
        .unwrap();
    assert_eq!(overlaid(&cache, &inbox, reply_uid).importance, Importance::High);

    // The session writes each delta record's properties through the same
    // path the item 5.4 sync uses, with categories already mapped to keys.
    let properties: Vec<RemoteMailProperties> = stored
        .iter()
        .filter_map(|(m, _)| {
            Some(RemoteMailProperties {
                message_id: m.message_id.clone()?,
                graph_id: m.graph_id.clone(),
                categories: m.categories.iter().map(|name| name.to_lowercase()).collect(),
                importance: m.importance,
                classification: m.classification,
                flag: m.flag.clone(),
            })
        })
        .collect();
    cache.apply_remote_mail_properties(&properties, &[], None, Utc::now()).unwrap();

    assert_eq!(
        overlaid(&cache, &inbox, reply_uid).importance,
        Importance::Normal,
        "Graph's value replaces the stale overlay"
    );
    let report = overlaid(&cache, &inbox, uid_of(&stored, "AAMkAGMSG1="));
    assert!(report.keywords.contains(&tag_keyword("work")), "categories surface as tag keywords");
    assert_eq!(report.importance, Importance::High);
}

#[test]
fn a_graph_move_keeps_the_uid_and_a_body_round_trips() {
    let account = temp_account();
    let cache = Cache::open(&account.0).unwrap();
    let stored = store_inbox(&cache, &account.0);
    let validity = cache.graph_uid_validity().unwrap();
    let inbox = graph_mailbox_id(&account.0, INBOX);
    let projects: MailboxId = graph_mailbox_id(&account.0, PROJECTS);
    let report_uid = uid_of(&stored, "AAMkAGMSG1=");

    // The body, with its attachments' metadata.
    let attachments_page = fixture(include_str!("../../../test-fixtures/outlook/message-attachments.json"));
    let attachments: Vec<_> = attachments_page["value"].as_array().unwrap().iter().filter_map(attachment_from_graph).collect();
    let body = email_body_from_graph(&fixture(include_str!("../../../test-fixtures/outlook/message-full.json")), report_uid, &attachments);
    cache.store_body(&inbox, report_uid, validity, &body).unwrap();
    assert_eq!(cache.load_body(&inbox, report_uid, validity).unwrap(), Some(body));

    // A move: the source folder's delta reports a removal, the target's an
    // upsert of the same immutable id, which maps to the same Uid.
    cache.delete_messages(&inbox, &[report_uid]).unwrap();
    let report = stored.iter().find(|(m, _)| m.graph_id == "AAMkAGMSG1=").unwrap().0.clone();
    let uid = cache.graph_uids(&[report.graph_id.as_str()]).unwrap()[0];
    assert_eq!(uid, report_uid);
    cache
        .upsert_messages(&projects, validity, &[email_summary_from_graph(&report, projects.clone(), uid)])
        .unwrap();

    assert!(cache.load_summary(&inbox, report_uid).unwrap().is_none());
    assert!(!cache.has_body(&inbox, report_uid, validity).unwrap(), "the source folder's body went with it");
    assert_eq!(cache.load_summary(&projects, report_uid).unwrap().unwrap().subject.as_deref(), Some("Quarterly report"));
    // The map entry survives the move, since a message still uses the Uid.
    assert_eq!(cache.prune_graph_uids().unwrap(), 0);
}
