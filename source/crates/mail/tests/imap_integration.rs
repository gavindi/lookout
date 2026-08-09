//! Protocol integration test against a real (ephemeral, local) mail server:
//! GreenMail, run via `testcontainers`. Exercises the actual
//! `run_account_session` actor - not a hand-rolled test harness - covering
//! LOGIN, folder discovery, envelope sync, draft save/replace/delete
//! (`CREATE` + `APPEND` + `UID SEARCH HEADER Message-Id` + `EXPUNGE`),
//! SMTP send, `APPEND` to Sent, and on-demand attachment part fetch
//! (`UID FETCH BODY.PEEK[<part>]` + transfer-decoding) against a message the
//! test itself APPENDs over a raw plain-TCP IMAP session.
//!
//! Requires Docker (or Podman with the Docker-compatible socket) and is
//! gated behind the `test-utils` feature (enabled automatically for this
//! crate's own dev-dependency build, see Cargo.toml). GreenMail's plain
//! ports aren't TLS, but our connector is TLS-only by design (matches real
//! providers), so this talks to GreenMail's IMAPS/SMTPS ports (3993/3465,
//! included in its default `-Dgreenmail.setup.test.all` setup) with the
//! `test-utils`-gated insecure certificate verifier (see
//! `connection::connect_tls_insecure_for_tests`) to accept its self-signed
//! cert - `LOOKOUT_INSECURE_TLS_FOR_TESTS` opts into that path. The raw
//! APPEND (below) uses GreenMail's plain IMAP port (3143) over plain TCP
//! instead - the same container, no TLS needed.
//!
//! NOTE: written carefully against documented GreenMail/testcontainers
//! behavior, but **not run in the environment that wrote it** (no Docker
//! available there) - treat a first real run as the actual validation, not
//! this comment.
//!
//! Run with:
//!   cargo test -p lookout-mail --features test-utils --test imap_integration -- --ignored

use std::sync::Arc;
use std::time::Duration;

use lookout_core::{AccountId, MailboxId};
use lookout_mail::session::{AccountCommand, AccountEvent, CredentialProvider};
use lookout_mail::{AccountConfig, ComposedMessage, Credential, EndpointConfig};
use testcontainers::core::{IntoContainerPort, WaitFor};
use testcontainers::runners::AsyncRunner;
use testcontainers::GenericImage;

struct FixedCredentialProvider;

#[async_trait::async_trait]
impl CredentialProvider for FixedCredentialProvider {
    async fn imap_credential(&self) -> Result<Credential, String> {
        Ok(Credential::Password("testpass".to_string()))
    }
    async fn smtp_credential(&self) -> Result<Credential, String> {
        Ok(Credential::Password("testpass".to_string()))
    }
}

#[tokio::test]
#[ignore = "requires Docker; run explicitly with `cargo test -- --ignored`"]
async fn logs_in_syncs_and_sends_against_a_real_imap_smtp_server() {
    // SAFETY (of intent, not memory): this only affects this crate's own
    // connect_tls() when compiled with the test-utils feature - see that
    // function's doc comment. Never set outside this test process.
    std::env::set_var("LOOKOUT_INSECURE_TLS_FOR_TESTS", "1");

    let container = GenericImage::new("greenmail/standalone", "2.1.11")
        .with_wait_for(WaitFor::message_on_stdout("Starting GreenMail standalone"))
        .with_exposed_port(3993.tcp())
        .with_exposed_port(3465.tcp())
        .with_exposed_port(3143.tcp())
        .start()
        .await
        .expect("failed to start GreenMail container - is Docker running?");

    let host = container.get_host().await.unwrap().to_string();
    let imaps_port = container.get_host_port_ipv4(3993).await.unwrap();
    let smtps_port = container.get_host_port_ipv4(3465).await.unwrap();
    let imap_plain_port = container.get_host_port_ipv4(3143).await.unwrap();

    // GreenMail's default setup runs with `-Dgreenmail.auth.disabled`: any
    // username/password authenticates and the mailbox is auto-created, so
    // no separate user-provisioning step is needed.
    let config = AccountConfig {
        account_id: AccountId("test-account".to_string()),
        display_name: "Test Account".to_string(),
        email: "testuser@localhost".to_string(),
        imap: EndpointConfig {
            host: host.clone(),
            port: imaps_port,
            use_tls: true,
            username: "testuser".to_string(),
        },
        smtp: EndpointConfig {
            host: host.clone(),
            port: smtps_port,
            use_tls: true,
            username: "testuser".to_string(),
        },
    };

    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();
    let credentials: Arc<dyn CredentialProvider> = Arc::new(FixedCredentialProvider);
    let handle = tokio::spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, evt_tx));

    // --- Wait for the account to come up: folder list + initial inbox sync.
    let folders = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::FoldersUpdated(_))).await;
    let AccountEvent::FoldersUpdated(folders) = folders else { unreachable!() };
    assert!(
        folders.iter().any(|f| matches!(f.role, lookout_core::MailboxRole::Inbox)),
        "expected an Inbox folder, got: {folders:?}"
    );
    wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::MessagesUpdated { .. })).await;

    // --- Draft round trip: save -> verify, replace -> verify still-one,
    // delete -> verify gone. Draft identity is a fixed stable Message-ID,
    // exactly what the composer's autosave uses.
    let drafts_id = MailboxId::new(&AccountId("test-account".to_string()), "Drafts");
    let draft_msg_id = "draft-integration@lookout.local".to_string();

    let draft = |subject: &str| ComposedMessage {
        from: "testuser@localhost".to_string(),
        display_name: None,
        to: vec!["testuser@localhost".to_string()],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        subject: subject.to_string(),
        text_body: "draft body".to_string(),
        html_body: None,
        calendar_part: None,
        read_receipt: None,
        request_read_receipt: false,
        in_reply_to: None,
        references: vec![],
        message_id: Some(draft_msg_id.clone()),
    };

    // First save: no Drafts mailbox exists yet, so the session CREATEs one.
    let _ = cmd_tx
        .send(AccountCommand::SaveDraft {
            msg: Box::new(draft("draft v1")),
            replace: false,
        })
        .await;
    let saved = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::DraftSaved { .. })).await;
    let AccountEvent::DraftSaved { message_id } = saved else { unreachable!() };
    assert_eq!(message_id, draft_msg_id);

    let _ = cmd_tx.send(AccountCommand::SyncMailbox(drafts_id.clone())).await;
    let listed = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::MessagesUpdated { mailbox, .. } if *mailbox == drafts_id)).await;
    let AccountEvent::MessagesUpdated { messages, .. } = listed else { unreachable!() };
    assert_eq!(messages.len(), 1, "expected exactly one draft after first save, got: {messages:?}");
    assert_eq!(messages[0].subject.as_deref(), Some("draft v1"));

    // Replace: same stable Message-ID, new content - must update in place,
    // not accumulate a second draft. `Refresh` forces a live IMAP re-sync of
    // the now-selected Drafts mailbox (a bare SyncMailbox would be answered
    // from the envelope cache and could mask a failed replace).
    let _ = cmd_tx
        .send(AccountCommand::SaveDraft {
            msg: Box::new(draft("draft v2")),
            replace: true,
        })
        .await;
    wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::DraftSaved { .. })).await;
    let _ = cmd_tx.send(AccountCommand::Refresh).await;
    let listed = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::MessagesUpdated { mailbox, .. } if *mailbox == drafts_id)).await;
    let AccountEvent::MessagesUpdated { messages, .. } = listed else { unreachable!() };
    assert_eq!(messages.len(), 1, "replace left duplicates behind: {messages:?}");
    assert_eq!(messages[0].subject.as_deref(), Some("draft v2"));

    // Delete (what Send does to a draft that was autosaved), then verify
    // the folder is empty.
    let _ = cmd_tx.send(AccountCommand::DeleteDraft { message_id: draft_msg_id.clone() }).await;
    let _ = cmd_tx.send(AccountCommand::Refresh).await;
    let listed = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::MessagesUpdated { mailbox, .. } if *mailbox == drafts_id)).await;
    let AccountEvent::MessagesUpdated { messages, .. } = listed else { unreachable!() };
    assert!(messages.is_empty(), "draft was not deleted: {messages:?}");

    // --- Send over SMTP + APPEND to Sent, as before.
    let msg = ComposedMessage {
        from: "testuser@localhost".to_string(),
        display_name: None,
        to: vec!["testuser@localhost".to_string()],
        cc: vec![],
        bcc: vec![],
        reply_to: vec![],
        subject: "integration test".to_string(),
        text_body: "hello from imap_integration.rs".to_string(),
        html_body: None,
        calendar_part: None,
        read_receipt: None,
        request_read_receipt: false,
        in_reply_to: None,
        references: vec![],
        message_id: None,
    };
    let _ = cmd_tx.send(AccountCommand::SendMessage(Box::new(msg))).await;
    wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::SendCompleted)).await;

    // --- On-demand attachment fetch. The session API can't APPEND an
    // arbitrary message, so seed one through a raw plain-TCP IMAP session:
    // a multipart/mixed message whose second part is a base64 attachment.
    let attachment_wire = b"JVBERi0xLjQKJWZha2UgcGRmIGJ5dGVzCg==".to_vec(); // base64 of "%PDF-1.4\n%fake pdf bytes\n"
    let attachment_bytes: &[u8] = b"%PDF-1.4\n%fake pdf bytes\n";
    let raw_message = concat!(
        "From: sender@example.com\r\n",
        "To: testuser@localhost\r\n",
        "Subject: with attachment\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/mixed; boundary=\"b0un7\"\r\n",
        "\r\n",
        "--b0un7\r\n",
        "Content-Type: text/plain; charset=utf-8\r\n",
        "Content-Transfer-Encoding: 7bit\r\n",
        "\r\n",
        "Body text here\r\n",
        "--b0un7\r\n",
        "Content-Type: application/pdf; name=\"doc.pdf\"\r\n",
        "Content-Disposition: attachment; filename=\"doc.pdf\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "\r\n",
    );
    let mut raw_message = raw_message.as_bytes().to_vec();
    raw_message.extend_from_slice(&attachment_wire);
    raw_message.extend_from_slice(b"\r\n--b0un7--\r\n");
    append_raw(&host, imap_plain_port, &raw_message).await;

    // A bare SyncMailbox would be answered from the envelope cache; Refresh
    // forces a live resync of the open mailbox (INBOX) so the new message
    // shows up.
    let _ = cmd_tx.send(AccountCommand::Refresh).await;
    let inbox_id = MailboxId::new(&AccountId("test-account".to_string()), "INBOX");
    let listed = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::MessagesUpdated { mailbox, .. } if *mailbox == inbox_id)).await;
    let AccountEvent::MessagesUpdated { messages, .. } = listed else { unreachable!() };
    let with_attachment = messages
        .iter()
        .find(|m| m.subject.as_deref() == Some("with attachment"))
        .unwrap_or_else(|| panic!("APPENDed message not synced: {messages:?}"));
    let structure = with_attachment
        .structure
        .as_ref()
        .unwrap_or_else(|| panic!("synced message has no BODYSTRUCTURE: {with_attachment:?}"));
    let part = structure
        .iter()
        .find(|p| p.filename.as_deref() == Some("doc.pdf"))
        .unwrap_or_else(|| panic!("attachment part not in structure: {structure:?}"));
    assert_eq!(part.part_number, "2");
    assert_eq!(part.content_type, "application/pdf");

    let _ = cmd_tx
        .send(AccountCommand::FetchAttachment {
            mailbox: inbox_id.clone(),
            uid: with_attachment.uid,
            part: part.clone(),
        })
        .await;
    let fetched = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::PartFetched { .. })).await;
    let AccountEvent::PartFetched {
        mailbox,
        uid,
        part: fetched_part,
        bytes,
    } = fetched
    else {
        unreachable!()
    };
    assert_eq!(mailbox, inbox_id);
    assert_eq!(uid, with_attachment.uid);
    assert_eq!(fetched_part.part_number, "2");
    // The wire bytes were base64; the actor must have decoded them.
    assert_eq!(bytes, attachment_bytes, "attachment bytes must be transfer-decoded");

    // --- Inline `cid:` image part fetch. A multipart/related message whose
    // second part is a base64 PNG carrying a Content-ID - the shape the
    // reading pane's cid: scheme handler resolves to this fetch. The part
    // must surface in the message's BODYSTRUCTURE-derived list with its cid
    // and a fetchable part number, and the fetch must return the
    // transfer-decoded PNG bytes.
    let png_wire = b"iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=";
    let raw_cid_message = concat!(
        "From: sender@example.com\r\n",
        "To: testuser@localhost\r\n",
        "Subject: inline cid image\r\n",
        "MIME-Version: 1.0\r\n",
        "Content-Type: multipart/related; boundary=\"c1d\"\r\n",
        "\r\n",
        "--c1d\r\n",
        "Content-Type: text/html; charset=utf-8\r\n",
        "Content-Transfer-Encoding: 7bit\r\n",
        "\r\n",
        "<html><body><img src=\"cid:logo123\"></body></html>\r\n",
        "--c1d\r\n",
        "Content-Type: image/png; name=\"logo.png\"\r\n",
        "Content-Disposition: inline; filename=\"logo.png\"\r\n",
        "Content-Transfer-Encoding: base64\r\n",
        "Content-ID: <logo123>\r\n",
        "\r\n",
    );
    let mut raw_cid_message = raw_cid_message.as_bytes().to_vec();
    raw_cid_message.extend_from_slice(png_wire);
    raw_cid_message.extend_from_slice(b"\r\n--c1d--\r\n");
    append_raw(&host, imap_plain_port, &raw_cid_message).await;

    let _ = cmd_tx.send(AccountCommand::Refresh).await;
    let listed = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::MessagesUpdated { mailbox, .. } if *mailbox == inbox_id)).await;
    let AccountEvent::MessagesUpdated { messages, .. } = listed else { unreachable!() };
    let cid_message = messages
        .iter()
        .find(|m| m.subject.as_deref() == Some("inline cid image"))
        .unwrap_or_else(|| panic!("APPENDed cid message not synced: {messages:?}"));
    let structure = cid_message
        .structure
        .as_ref()
        .unwrap_or_else(|| panic!("cid message has no BODYSTRUCTURE: {cid_message:?}"));
    let image_part = structure
        .iter()
        .find(|p| p.cid.as_deref() == Some("logo123"))
        .unwrap_or_else(|| panic!("inline image part not in structure: {structure:?}"));
    assert_eq!(image_part.part_number, "2");
    assert_eq!(image_part.content_type, "image/png");
    // Inline, not an attachment: the strip must not list it.
    assert!(!image_part.is_attachment);
    assert!(
        structure.iter().all(|p| !p.is_attachment),
        "no parts of the related body may count as attachments: {structure:?}"
    );

    let _ = cmd_tx
        .send(AccountCommand::FetchAttachment {
            mailbox: inbox_id.clone(),
            uid: cid_message.uid,
            part: image_part.clone(),
        })
        .await;
    let fetched = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::PartFetched { .. })).await;
    let AccountEvent::PartFetched { bytes, .. } = fetched else { unreachable!() };
    // A PNG's magic header, proving the base64 wire bytes were decoded.
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "cid image bytes must be transfer-decoded");
    assert!(bytes.len() > 8);

    // --- Whole-message .eml export: `FetchRawMessage` returns the message's
    // raw RFC 5322 bytes verbatim (the shape an .eml export writes to disk,
    // `\Seen` never set since the fetch is `BODY.PEEK[]`), and a second
    // request is served from the flat-file raw-message cache with the same
    // bytes.
    let _ = cmd_tx
        .send(AccountCommand::FetchRawMessage {
            mailbox: inbox_id.clone(),
            uid: with_attachment.uid,
        })
        .await;
    let fetched_raw = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::RawMessageFetched { .. })).await;
    let AccountEvent::RawMessageFetched {
        mailbox: fetched_mailbox,
        uid: fetched_uid,
        bytes,
    } = fetched_raw
    else {
        unreachable!()
    };
    assert_eq!(fetched_mailbox, inbox_id);
    assert_eq!(fetched_uid, with_attachment.uid);
    assert_eq!(bytes, raw_message, "whole-message fetch must return the raw bytes verbatim");

    let _ = cmd_tx
        .send(AccountCommand::FetchRawMessage {
            mailbox: inbox_id.clone(),
            uid: with_attachment.uid,
        })
        .await;
    let second = wait_for_event(&evt_rx, |e| matches!(e, AccountEvent::RawMessageFetched { .. })).await;
    let AccountEvent::RawMessageFetched { bytes: second_bytes, .. } = second else {
        unreachable!()
    };
    assert_eq!(second_bytes, raw_message, "cached raw message must match the original");

    let _ = cmd_tx.send(AccountCommand::Shutdown).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;
}

/// Opens a plain-TCP IMAP session to GreenMail (auth is disabled in its test
/// setup, so any credentials authenticate) and `APPEND`s `raw` to INBOX.
async fn append_raw(host: &str, port: u16, raw: &[u8]) {
    let tcp = tokio::net::TcpStream::connect((host, port)).await.expect("plain IMAP connect");
    let client = async_imap::Client::new(tcp);
    let mut session = client.login("testuser", "testpass").await.map_err(|e| e.0).expect("plain IMAP login");
    session.append("INBOX", None, None, raw).await.expect("APPEND to INBOX");
    session.logout().await.expect("plain IMAP logout");
}

/// Drains events until one matches `pred` (discarding the rest), panicking
/// on timeout, channel close, or an `Error` event - the session reports
/// failures that way and the test should fail loudly on any of them.
async fn wait_for_event<F>(evt_rx: &async_channel::Receiver<AccountEvent>, pred: F) -> AccountEvent
where
    F: Fn(&AccountEvent) -> bool,
{
    let deadline = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(deadline);
    loop {
        tokio::select! {
            _ = &mut deadline => panic!("timed out waiting for a matching account event"),
            event = evt_rx.recv() => {
                let Ok(event) = event else { panic!("account session channel closed unexpectedly") };
                if let AccountEvent::Error(e) = &event {
                    panic!("account session reported an error: {e}");
                }
                if pred(&event) {
                    return event;
                }
            }
        }
    }
}
