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
use lookout_core::{parse_imip_method, AccountId, ImipMethod, MailboxId, SystemFlagBit, Uid};
use lookout_graph::{graph_mailbox_id, run_graph_mail_session, GraphClient, GraphMailSessionConfig, MailClient, TokenProvider};
use lookout_mail::body::parse_body;
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
    // The Inbox's messages. The delta link points back at this server, so the
    // next sync proves the persisted cursor is followed rather than the whole
    // folder being read again.
    let mut message_delta = fixture(include_str!("../../../test-fixtures/outlook/message-summary-delta.json"));
    message_delta["@odata.deltaLink"] = json!(format!("{}/v1.0/graph-mail-continuation", server.uri()));
    Mock::given(method("GET"))
        .and(path("/v1.0/me/mailFolders/AAMkAGINBOX%3D/messages/delta"))
        .respond_with(ResponseTemplate::new(200).set_body_json(message_delta))
        .mount(server)
        .await;
    // The incremental page the persisted cursor addresses (item 5.7.8):
    // nothing changed, but the request itself is what the test asserts.
    Mock::given(method("GET"))
        .and(path("/v1.0/graph-mail-continuation"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "value": [],
            "@odata.deltaLink": format!("{}/v1.0/graph-mail-continuation-2", server.uri()),
        })))
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
    // The Inbox's next delta cursor is persisted (item 5.7.8), so a restart
    // resumes incrementally under the same Uids instead of re-reading the
    // whole folder.
    assert!(cache.graph_folder_cursor(&inbox).unwrap().is_some(), "the Inbox delta cursor is persisted");

    // A later sync follows that cursor instead of re-reading the folder. The
    // continuation page is empty, but reaching it is the proof.
    commands.send(AccountCommand::SyncMailbox(inbox.clone())).await.unwrap();
    loop {
        // The handler emits a cached MessagesUpdated before it syncs, so wait
        // for the Busy/…/Idle pair to know the cursor request has completed.
        if matches!(next_event(&event_rx).await, AccountEvent::ConnectionStateChanged(ConnectionState::Idle)) {
            break;
        }
    }
    let requests = server.received_requests().await.expect("the fake server records requests");
    assert!(
        requests.iter().any(|request| request.url.path() == "/v1.0/graph-mail-continuation"),
        "the second sync followed the persisted delta cursor"
    );

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

    // The cursor survives the session and a fresh cache handle, so the next
    // launch reads only the folder's changes.
    drop(cache);
    let reopened = Cache::open(&account_id).unwrap();
    assert!(reopened.graph_folder_cursor(&inbox).unwrap().is_some(), "the persisted cursor survives a reopen");
    drop(reopened);
    let _ = remove_account_cache(&account_id);
}

/// The MIME path is the transport's fidelity boundary: Graph's `/$value` bytes
/// must parse exactly like an IMAP body, keeping the iTIP invite and the
/// Authentication-Results headers (R3 / item 5.7.1). These are the same
/// checked-in fixtures the IMAP-side reader tests use.
#[tokio::test]
async fn graph_mime_fixtures_keep_invites_and_authentication_results() {
    let server = MockServer::start().await;
    for (id, raw) in [
        ("invite", include_bytes!("../../../test-fixtures/outlook/invite.eml").as_slice()),
        ("auth", include_bytes!("../../../test-fixtures/outlook/auth-results.eml").as_slice()),
    ] {
        Mock::given(method("GET"))
            .and(path(format!("/v1.0/me/messages/{id}/$value")))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(raw.to_vec()))
            .mount(&server)
            .await;
    }

    let client = MailClient::new(GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri())));

    let invite = client.get_mime("invite").await.unwrap().expect("the invite MIME");
    let invite = parse_body(Uid(0), &invite).expect("the Graph MIME bytes parse as a message");
    let ics = invite.calendar_ics.as_deref().expect("the invite keeps its text/calendar part");
    assert_eq!(parse_imip_method(ics), ImipMethod::Request);
    assert!(ics.contains("X-MICROSOFT-CDO-BUSYSTATUS:BUSY"), "Outlook extensions survive");

    let auth = client.get_mime("auth").await.unwrap().expect("the auth-results MIME");
    let auth = parse_body(Uid(0), &auth).expect("the Graph MIME bytes parse as a message");
    let results = auth.auth_results.as_ref().expect("Authentication-Results survives");
    assert!(results.any_failure(), "the fixture's failing verdict is readable");
}
