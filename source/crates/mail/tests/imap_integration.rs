//! Protocol integration test against a real (ephemeral, local) mail server:
//! GreenMail, run via `testcontainers`. Exercises the actual
//! `run_account_session` actor - not a hand-rolled test harness - covering
//! LOGIN, folder discovery, envelope sync, SMTP send, and `APPEND` to Sent.
//!
//! Requires Docker (or Podman with the Docker-compatible socket) and is
//! gated behind the `test-utils` feature (enabled automatically for this
//! crate's own dev-dependency build, see Cargo.toml). GreenMail's plain
//! ports aren't TLS, but our connector is TLS-only by design (matches real
//! providers), so this talks to GreenMail's IMAPS/SMTPS ports (3993/3465,
//! included in its default `-Dgreenmail.setup.test.all` setup) with the
//! `test-utils`-gated insecure certificate verifier (see
//! `connection::connect_tls_insecure_for_tests`) to accept its self-signed
//! cert - `LOOKOUT_INSECURE_TLS_FOR_TESTS` opts into that path.
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

use lookout_core::AccountId;
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
        .start()
        .await
        .expect("failed to start GreenMail container - is Docker running?");

    let host = container.get_host().await.unwrap().to_string();
    let imaps_port = container.get_host_port_ipv4(3993).await.unwrap();
    let smtps_port = container.get_host_port_ipv4(3465).await.unwrap();

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
            host,
            port: smtps_port,
            use_tls: true,
            username: "testuser".to_string(),
        },
    };

    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();
    let credentials: Arc<dyn CredentialProvider> = Arc::new(FixedCredentialProvider);
    let handle = tokio::spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, evt_tx));

    let mut got_folders = false;
    let mut got_messages = false;
    let mut got_send = false;
    let mut send_requested = false;
    let deadline = tokio::time::sleep(Duration::from_secs(60));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => panic!("timed out waiting for folders/messages/send"),
            event = evt_rx.recv() => {
                let Ok(event) = event else { panic!("account session channel closed unexpectedly") };
                match event {
                    AccountEvent::FoldersUpdated(folders) => {
                        assert!(
                            folders.iter().any(|f| matches!(f.role, lookout_core::MailboxRole::Inbox)),
                            "expected an Inbox folder, got: {folders:?}"
                        );
                        got_folders = true;
                    }
                    AccountEvent::MessagesUpdated { .. } => {
                        got_messages = true;
                        if !send_requested {
                            let msg = ComposedMessage {
                                from: "testuser@localhost".to_string(),
                                to: vec!["testuser@localhost".to_string()],
                                cc: vec![],
                                bcc: vec![],
                                subject: "integration test".to_string(),
                                text_body: "hello from imap_integration.rs".to_string(),
                                html_body: None,
                                in_reply_to: None,
                                references: vec![],
                            };
                            let _ = cmd_tx.send(AccountCommand::SendMessage(msg)).await;
                            send_requested = true;
                        }
                    }
                    AccountEvent::SendCompleted => {
                        got_send = true;
                    }
                    AccountEvent::Error(e) => panic!("account session reported an error: {e}"),
                    AccountEvent::ConnectionStateChanged(_)
                    | AccountEvent::BodyFetched { .. }
                    | AccountEvent::MessageMoved { .. }
                    | AccountEvent::MessageSnoozed => {}
                }
                if got_folders && got_messages && got_send {
                    break;
                }
            }
        }
    }

    let _ = cmd_tx.send(AccountCommand::Shutdown).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

    assert!(got_folders && got_messages && got_send);
}
