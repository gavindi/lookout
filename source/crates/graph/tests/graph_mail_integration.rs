//! Read-only live-tenant integration harness for the Graph mail session
//! (MAIL_GRAPH_OVERVIEW.md item 5.7.9, parallel to
//! `lookout-mail/tests/imap_integration.rs`).
//!
//! The rest of the Graph mail suite uses wiremock and checked-in fixtures.
//! This harness is the one test that talks to Microsoft Graph itself, because
//! the open questions in R3/R5 (MIME fidelity and real delta cost) cannot be
//! answered from fixtures. It is deliberately read-only: it signs in with a
//! token supplied by the caller, syncs the folder hierarchy and Inbox, and
//! fetches one body, so it can be run against a production mailbox without
//! sending, moving or flagging anything.
//!
//! It has no OAuth flow of its own, matching the crate's `TokenProvider` seam:
//! obtain a Graph access token with `Mail.ReadWrite` (the token used by the
//! app for an opted-in account works), then run:
//!
//! ```text
//! LOOKOUT_GRAPH_MAIL_TOKEN='eyJ0…' \
//!   cargo test -p lookout-graph --test graph_mail_integration -- --ignored --nocapture
//! ```
//!
//! `LOOKOUT_GRAPH_BASE_URL` can point at a sovereign cloud service root; it
//! defaults to the public `https://graph.microsoft.com/v1.0`.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use lookout_core::{AccountId, MailboxRole};
use lookout_graph::{run_graph_mail_session, GraphClient, GraphMailSessionConfig, MailClient, TokenProvider, GRAPH_BASE_URL};
use lookout_mail::session::{AccountCommand, AccountEvent, ConnectionState};
use lookout_mail::{remove_account_cache, Cache};

struct EnvToken(String);

#[async_trait]
impl TokenProvider for EnvToken {
    async fn graph_token(&self) -> lookout_graph::Result<String> {
        Ok(self.0.clone())
    }

    async fn ensure_scopes(&self, _scopes: &[&str]) -> lookout_graph::Result<()> {
        Ok(())
    }
}

/// Removes the throwaway account cache even if an assertion panics.
struct TempAccount(AccountId);

impl Drop for TempAccount {
    fn drop(&mut self) {
        let _ = remove_account_cache(&self.0);
    }
}

fn temp_account() -> TempAccount {
    TempAccount(AccountId(format!(
        "/test/graph_mail_live_{}_{}",
        std::process::id(),
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos()
    )))
}

async fn next_event(events: &async_channel::Receiver<AccountEvent>) -> AccountEvent {
    tokio::time::timeout(Duration::from_secs(90), events.recv())
        .await
        .expect("the live Graph session went quiet for 90 seconds")
        .expect("the event channel closed")
}

#[tokio::test]
#[ignore = "requires LOOKOUT_GRAPH_MAIL_TOKEN from a live Microsoft 365 mailbox"]
async fn syncs_a_live_mailbox_read_only() {
    let token = std::env::var("LOOKOUT_GRAPH_MAIL_TOKEN").expect("set LOOKOUT_GRAPH_MAIL_TOKEN to a Graph access token with Mail.ReadWrite");
    let token = token.trim().to_string();
    assert!(!token.is_empty(), "LOOKOUT_GRAPH_MAIL_TOKEN is empty");
    let base_url = std::env::var("LOOKOUT_GRAPH_BASE_URL").unwrap_or_else(|_| GRAPH_BASE_URL.to_string());
    let account = temp_account();

    let client = MailClient::new(GraphClient::with_base_url(Arc::new(EnvToken(token)), base_url));
    let config = GraphMailSessionConfig {
        account_id: account.0.clone(),
        zone: None,
        // One hour: the harness exercises the startup sync, not the poll loop.
        poll_interval: Duration::from_secs(60 * 60),
        mention_addresses: Vec::new(),
        mention_names: Vec::new(),
    };

    let (commands, command_rx) = async_channel::unbounded();
    let (_interactive, interactive_rx) = async_channel::unbounded();
    let (events, event_rx) = async_channel::unbounded();
    let session = tokio::spawn(run_graph_mail_session(config, client, command_rx, interactive_rx, events));

    // The folder hierarchy must come back before anything else can be checked.
    let folders = loop {
        match next_event(&event_rx).await {
            AccountEvent::FoldersUpdated(folders) if !folders.is_empty() => break folders,
            AccountEvent::ConnectionStateChanged(ConnectionState::Error { message, .. }) => {
                panic!("live Graph folder sync failed: {message}")
            }
            _ => {}
        }
    };
    let inbox = folders
        .iter()
        .find(|folder| folder.role == MailboxRole::Inbox)
        .map(|folder| folder.id.clone())
        .expect("the live mailbox has an Inbox");

    // The initial Inbox messages delta. An empty Inbox is a valid answer.
    let messages = loop {
        match next_event(&event_rx).await {
            AccountEvent::MessagesUpdated { mailbox, messages } if mailbox == inbox => break messages,
            AccountEvent::ConnectionStateChanged(ConnectionState::Error { message, .. }) => {
                panic!("live Graph Inbox sync failed: {message}")
            }
            _ => {}
        }
    };

    // One body read checks the `/$value` MIME path (and its JSON fallback)
    // against real Graph bytes. This is the read R3 could not settle offline.
    if let Some(first) = messages.first() {
        let uid = first.uid;
        commands.send(AccountCommand::FetchBody { mailbox: inbox.clone(), uid }).await.unwrap();
        let body = loop {
            match next_event(&event_rx).await {
                AccountEvent::BodyFetched { uid: fetched, body, .. } if fetched == uid => break body,
                AccountEvent::ConnectionStateChanged(ConnectionState::Error { message, .. }) => {
                    panic!("live Graph body read failed: {message}")
                }
                _ => {}
            }
        };
        assert!(
            body.text_body.is_some() || body.html_body.is_some() || !body.parts.is_empty(),
            "the live Graph body read produced no text, HTML or parts"
        );
    }

    commands.send(AccountCommand::Shutdown).await.unwrap();
    tokio::time::timeout(Duration::from_secs(30), session)
        .await
        .expect("the live Graph session stopped")
        .unwrap();

    // The same cache the app reads is populated, so the session really used
    // the shared transport seam rather than a private store.
    let cache = Cache::open(&account.0).unwrap();
    assert!(!cache.load_mailboxes(&account.0).unwrap().is_empty());
    assert_eq!(cache.load_messages(&inbox).unwrap().len(), messages.len(), "the Inbox cache matches the sync event");
}
