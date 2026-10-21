//! The Graph mail session actor (MAIL_GRAPH_OVERVIEW.md item 5.7.5): a fake
//! Graph server drives `run_graph_mail_session` through its first folder and
//! message sync, an on-demand body read and a native read-state write, and the
//! test checks the cache and the emitted `AccountEvent`s - the same seam the
//! IMAP session speaks.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lookout_core::{AccountId, MailboxId, SystemFlagBit, Uid};
use lookout_graph::{graph_mailbox_id, run_graph_mail_session, GraphClient, GraphMailSessionConfig, MailClient, TokenProvider};
use lookout_mail::session::{AccountCommand, AccountEvent, ConnectionState};
use lookout_mail::{remove_account_cache, Cache};
use serde_json::{json, Value};
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const INBOX: &str = "AAMkAGINBOX=";
const REPORT: &str = "AAMkAGMSG1=";

struct StubTokens;

#[async_trait]
impl TokenProvider for StubTokens {
    async fn graph_token(&self) -> lookout_graph::Result<String> {
        Ok("stub".to_string())
    }
    async fn ensure_scopes(&self, _scopes: &[&str]) -> lookout_graph::Result<()> {
        Ok(())
    }
}

fn fixture(text: &str) -> Value {
    serde_json::from_str(text).unwrap()
}

fn temp_account() -> AccountId {
    AccountId(format!(
        "/test/graph_mail_session_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    ))
}

async fn next_event(events: &async_channel::Receiver<AccountEvent>) -> AccountEvent {
    tokio::time::timeout(Duration::from_secs(10), events.recv())
        .await
        .expect("the session went quiet")
        .expect("the event channel closed")
}

async fn mount_graph(server: &MockServer) {
    // The whole folder hierarchy, through the delta endpoint. The fixture's
    // cursor points at the public cloud; the session only follows it on a
    // later poll, which this test's hour-long interval never reaches.
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(include_str!("../../../test-fixtures/outlook/mail-folders-delta.json"))))
        .mount(server)
        .await;
    // Well-known names: Sent and Trash exist, Drafts/Junk/Archive don't.
    for (name, id) in [("inbox", INBOX), ("sentitems", "AAMkAGSENT="), ("deleteditems", "AAMkAGDELETED=")] {
        Mock::given(method("GET"))
            .and(path(format!("/v1.0/me/mailFolders/{name}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": id })))
            .mount(server)
            .await;
    }
    for name in ["drafts", "junkemail", "archive"] {
        Mock::given(method("GET"))
            .and(path(format!("/v1.0/me/mailFolders/{name}")))
            .respond_with(ResponseTemplate::new(404))
            .mount(server)
            .await;
    }
    // The Inbox's messages.
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/AAMkAGINBOX%3D/messages/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(include_str!("../../../test-fixtures/outlook/message-summary-delta.json"))))
        .mount(server)
        .await;
    // One message read: JSON row, attachments, then the raw MIME.
    Mock::given(method("GET"))
        .and(path("/v1.0/me/messages/AAMkAGMSG1%3D"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(include_str!("../../../test-fixtures/outlook/message-full.json"))))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/messages/AAMkAGMSG1%3D/attachments"))
        .respond_with(ResponseTemplate::new(200).set_body_json(fixture(include_str!("../../../test-fixtures/outlook/message-attachments.json"))))
        .mount(server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v1.0/me/messages/AAMkAGMSG1%3D/$value"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(
            b"MIME-Version: 1.0\r\nFrom: Adele Vance <adelev@contoso.com>\r\nTo: team@contoso.com\r\nSubject: Quarterly report\r\nContent-Type: text/plain; charset=\"utf-8\"\r\n\r\nHi team, the quarterly numbers are in.\r\n"
                .to_vec(),
        ))
        .mount(server)
        .await;
    // The native read-state write.
    Mock::given(method("PATCH"))
        .and(path("/v1.0/me/messages/AAMkAGMSG1%3D"))
        .and(body_partial_json(json!({ "isRead": true })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": REPORT })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_graph_session_syncs_reads_and_writes_native_properties() {
    let account_id = temp_account();
    let server = MockServer::start().await;
    mount_graph(&server).await;

    let client = MailClient::new(GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri())));
    let config = GraphMailSessionConfig {
        account_id: account_id.clone(),
        zone: None,
        poll_interval: Duration::from_secs(60 * 60),
        mention_addresses: Vec::new(),
        mention_names: Vec::new(),
    };
    let (commands, command_rx) = async_channel::unbounded();
    let (interactive, interactive_rx) = async_channel::unbounded();
    let (events, event_rx) = async_channel::unbounded();
    let session = tokio::spawn(run_graph_mail_session(config, client, command_rx, interactive_rx, events));

    // The initial sync ends with Idle (after the Inbox's MessagesUpdated).
    loop {
        if matches!(next_event(&event_rx).await, AccountEvent::ConnectionStateChanged(ConnectionState::Idle)) {
            break;
        }
    }

    // The folder tree and the Inbox rows landed in the shared cache.
    let cache = Cache::open(&account_id).unwrap();
    let inbox: MailboxId = graph_mailbox_id(&account_id, INBOX);
    let folders = cache.load_mailboxes(&account_id).unwrap();
    assert_eq!(folders.len(), 4, "the fixture's live folders, not the tombstone");
    assert_eq!(folders.iter().filter(|folder| folder.role == lookout_core::MailboxRole::Inbox).count(), 1);
    let messages = cache.load_messages(&inbox).unwrap();
    assert_eq!(messages.len(), 3, "the Inbox rows");
    let report = messages.iter().find(|message| message.message_id.as_deref() == Some("<report-1@contoso.com>")).unwrap();
    assert!(report.is_unread());
    assert!(report.is_flagged_for_follow_up(), "the Graph flag rode the 5.4 overlay");
    assert!(report.keywords.contains(&lookout_core::tag_keyword("work")), "the category rode the 5.4 overlay");
    let report_uid: Uid = report.uid;

    // An on-demand body read uses the MIME `$value` and keeps Graph attachment
    // ids as part numbers.
    interactive
        .send(AccountCommand::FetchBody {
            mailbox: inbox.clone(),
            uid: report_uid,
        })
        .await
        .unwrap();
    let body = loop {
        if let AccountEvent::BodyFetched { body, .. } = next_event(&event_rx).await {
            break body;
        }
    };
    assert_eq!(body.uid, report_uid);
    assert!(body.text_body.as_deref().unwrap_or_default().contains("quarterly numbers"));
    assert_eq!(body.parts.len(), 2, "the attachment list");
    assert!(body.parts.iter().any(|part| part.filename.as_deref() == Some("Q3-report.pdf")));

    // Mark-as-read is a native `PATCH isRead`.
    commands
        .send(AccountCommand::StoreFlags {
            mailbox: inbox.clone(),
            uid: report_uid,
            add: vec![SystemFlagBit::Seen],
            remove: Vec::new(),
        })
        .await
        .unwrap();
    loop {
        if let AccountEvent::MessagesUpdated { .. } = next_event(&event_rx).await {
            break;
        }
    }
    let report = cache.load_messages(&inbox).unwrap().into_iter().find(|message| message.uid == report_uid).unwrap();
    assert!(report.flags.contains(&SystemFlagBit::Seen), "the cache was patched after the PATCH");

    commands.send(AccountCommand::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), session).await.expect("the session stopped").unwrap();

    drop(cache);
    let _ = remove_account_cache(&account_id);
}
