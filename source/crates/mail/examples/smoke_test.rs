//! Manual end-to-end verification: drives the real `run_account_session`
//! actor (LOGIN via XOAUTH2, LIST, SELECT INBOX, UID FETCH envelopes, then
//! IDLE) against a live Gmail account discovered through GOA. Entirely
//! read-only - no APPEND/STORE/EXPUNGE. Run with:
//!   cargo run -p lookout-mail --example smoke_test

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;
use std::time::Duration;

use lookout_core::AccountId;
use lookout_goa::{AuthMethod, GoaClient, GoaMailAccount};
use lookout_mail::session::{AccountCommand, AccountEvent, CredentialProvider};
use lookout_mail::{AccountConfig, Credential, EndpointConfig};

struct GoaCredentialProvider {
    client: GoaClient,
    account: GoaMailAccount,
}

#[async_trait::async_trait]
impl CredentialProvider for GoaCredentialProvider {
    async fn imap_credential(&self) -> Result<Credential, String> {
        self.client.ensure_credentials(&self.account).await.map_err(|e| e.to_string())?;
        match &self.account.auth {
            AuthMethod::OAuth2 => {
                let (token, _expires_in) = self.client.get_access_token(&self.account).await.map_err(|e| e.to_string())?;
                Ok(Credential::OAuth2AccessToken(token))
            }
            AuthMethod::Password { .. } => {
                let password = self.client.get_imap_password(&self.account).await.map_err(|e| e.to_string())?;
                Ok(Credential::Password(password))
            }
        }
    }

    async fn smtp_credential(&self) -> Result<Credential, String> {
        self.client.ensure_credentials(&self.account).await.map_err(|e| e.to_string())?;
        match &self.account.auth {
            AuthMethod::OAuth2 => {
                let (token, _expires_in) = self.client.get_access_token(&self.account).await.map_err(|e| e.to_string())?;
                Ok(Credential::OAuth2AccessToken(token))
            }
            AuthMethod::Password { .. } => {
                let password = self.client.get_smtp_password(&self.account).await.map_err(|e| e.to_string())?;
                Ok(Credential::Password(password))
            }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let goa = GoaClient::connect().await?;
    let accounts = goa.list_mail_accounts().await?;
    let account = accounts.into_iter().next().ok_or_else(|| anyhow::anyhow!("no GOA mail accounts found"))?;
    println!("Using account: {} <{}>", account.display_name, account.email);

    let config = AccountConfig {
        account_id: AccountId(account.account_id.0.clone()),
        display_name: account.display_name.clone(),
        email: account.email.clone(),
        imap: EndpointConfig {
            host: account.imap.host.clone(),
            port: account.imap.port.unwrap_or(993),
            use_tls: account.imap.use_tls,
            username: account.imap.username.clone(),
        },
        smtp: EndpointConfig {
            host: account.smtp.host.clone(),
            port: account.smtp.port.unwrap_or(587),
            use_tls: account.smtp.use_tls,
            username: account.smtp.username.clone(),
        },
    };

    let credentials: Arc<dyn CredentialProvider> = Arc::new(GoaCredentialProvider { client: goa, account });

    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (_interactive_tx, interactive_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();

    let handle = tokio::spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, interactive_rx, evt_tx));

    let mut got_folders = false;
    let mut got_messages = false;
    let mut got_body = false;
    let mut body_requested = false;
    let deadline = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                println!("Timed out waiting for folders/messages/body");
                break;
            }
            event = evt_rx.recv() => {
                let Ok(event) = event else { break };
                match event {
                    AccountEvent::ConnectionStateChanged(state) => println!("state -> {state:?}"),
                    AccountEvent::FoldersUpdated(folders) => {
                        println!("folders ({}):", folders.len());
                        for f in &folders {
                            println!("  {:?}  {} (unread={} total={})", f.role, f.name, f.unread, f.total);
                        }
                        got_folders = true;
                    }
                    AccountEvent::MessagesUpdated { mailbox, messages } => {
                        println!("messages in {mailbox} ({}):", messages.len());
                        for m in messages.iter().rev().take(5) {
                            println!(
                                "  uid={} unread={} subject={:?} from={:?}",
                                m.uid.0,
                                m.is_unread(),
                                m.subject,
                                m.from.first().map(|a| a.display_label().to_string())
                            );
                        }
                        got_messages = true;
                        if !body_requested {
                            if let Some(newest) = messages.iter().max_by_key(|m| m.date) {
                                println!("requesting body for uid={}", newest.uid.0);
                                let _ = cmd_tx
                                    .send(AccountCommand::FetchBody { mailbox: mailbox.clone(), uid: newest.uid })
                                    .await;
                                body_requested = true;
                            }
                        }
                    }
                    AccountEvent::BodyFetched { uid, body, .. } => {
                        println!(
                            "body for uid={}: text_len={:?} html_len={:?} parts={} headers={}",
                            uid.0,
                            body.text_body.as_ref().map(|s| s.len()),
                            body.html_body.as_ref().map(|s| s.len()),
                            body.parts.len(),
                            body.headers.len(),
                        );
                        if let Some(text) = &body.text_body {
                            let preview: String = text.chars().take(120).collect();
                            println!("  text preview: {preview:?}");
                        }
                        got_body = true;
                    }
                    AccountEvent::SendCompleted => println!("send completed"),
                    AccountEvent::SendFailed(e) => println!("send failed: {e}"),
                    AccountEvent::DraftSaved { message_id } => println!("draft saved: {message_id}"),
                    AccountEvent::MessageMoved { .. } | AccountEvent::MessageSnoozed | AccountEvent::MailboxExpunged { .. } => {}
                    AccountEvent::PreviewsFetched { .. } => {}
                    AccountEvent::NewMessages { .. } => {}
                    AccountEvent::SearchResults { .. } => {}
                    AccountEvent::PartFetched { part, bytes, .. } => println!("part {} fetched ({} bytes)", part.part_number, bytes.len()),
                    AccountEvent::PartFetchFailed { part_number, message, .. } => println!("part {part_number} fetch failed: {message}"),
                    AccountEvent::RawMessageFetched { uid, bytes, .. } => println!("raw message {} fetched ({} bytes)", uid.0, bytes.len()),
                    AccountEvent::RawMessageFetchFailed { uid, message, .. } => println!("raw message {} fetch failed: {message}", uid.0),
                    AccountEvent::MoveFailed { .. } => println!("move failed"),
                    AccountEvent::StoreFlagsFailed { .. } => println!("store flags failed"),
                    AccountEvent::Error(e) => println!("ERROR: {e}"),
                }
                if got_folders && got_messages && got_body {
                    println!("Got folders, messages, and a fetched body - smoke test passed. Shutting down.");
                    break;
                }
            }
        }
    }

    let _ = cmd_tx.send(AccountCommand::Shutdown).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

    if got_folders && got_messages && got_body {
        Ok(())
    } else {
        anyhow::bail!("smoke test incomplete: got_folders={got_folders} got_messages={got_messages} got_body={got_body}")
    }
}
