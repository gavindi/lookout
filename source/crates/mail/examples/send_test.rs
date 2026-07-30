//! Manual end-to-end verification of the SMTP send path: sends a real,
//! self-addressed test email (account -> itself) through the live
//! `run_account_session` actor, confirms `SendCompleted`, then re-syncs the
//! Sent mailbox and checks the message landed there via `APPEND`. Run with:
//!   cargo run -p lookout-mail --example send_test

use std::sync::Arc;
use std::time::Duration;

use lookout_core::AccountId;
use lookout_goa::{AuthMethod, GoaClient, GoaMailAccount};
use lookout_mail::session::{AccountCommand, AccountEvent, CredentialProvider};
use lookout_mail::{AccountConfig, ComposedMessage, Credential, EndpointConfig};

struct GoaCredentialProvider {
    client: GoaClient,
    account: GoaMailAccount,
}

#[async_trait::async_trait]
impl CredentialProvider for GoaCredentialProvider {
    async fn imap_credential(&self) -> Result<Credential, String> {
        self.credential().await
    }
    async fn smtp_credential(&self) -> Result<Credential, String> {
        self.credential().await
    }
}

impl GoaCredentialProvider {
    async fn credential(&self) -> Result<Credential, String> {
        self.client.ensure_credentials(&self.account).await.map_err(|e| e.to_string())?;
        match &self.account.auth {
            AuthMethod::OAuth2 => {
                let (token, _) = self.client.get_access_token(&self.account).await.map_err(|e| e.to_string())?;
                Ok(Credential::OAuth2AccessToken(token))
            }
            AuthMethod::Password { .. } => Ok(Credential::Password(self.client.get_imap_password(&self.account).await.map_err(|e| e.to_string())?)),
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
    let self_address = account.email.clone();
    let credentials: Arc<dyn CredentialProvider> = Arc::new(GoaCredentialProvider { client: goa, account });

    let (cmd_tx, cmd_rx) = async_channel::unbounded();
    let (evt_tx, evt_rx) = async_channel::unbounded();
    let handle = tokio::spawn(lookout_mail::session::run_account_session(config, credentials, cmd_rx, evt_tx));

    let marker = format!("Lookout send_test {}", uuid::Uuid::new_v4());
    println!("Sending self-addressed test email, subject: {marker:?}");

    let mut sent_command = false;
    let mut send_completed = false;
    let mut sent_mailbox_id = None;
    let deadline = tokio::time::sleep(Duration::from_secs(30));
    tokio::pin!(deadline);

    loop {
        tokio::select! {
            _ = &mut deadline => {
                println!("Timed out");
                break;
            }
            event = evt_rx.recv() => {
                let Ok(event) = event else { break };
                match event {
                    AccountEvent::ConnectionStateChanged(state) => println!("state -> {state:?}"),
                    AccountEvent::FoldersUpdated(folders) => {
                        if !sent_command {
                            let msg = ComposedMessage {
                                from: self_address.clone(),
                                to: vec![self_address.clone()],
                                cc: vec![],
                                bcc: vec![],
                                subject: marker.clone(),
                                text_body: "This is an automated self-test from Lookout's send_test example.".into(),
                                in_reply_to: None,
                                references: vec![],
                            };
                            let _ = cmd_tx.send(AccountCommand::SendMessage(msg)).await;
                            sent_command = true;
                        }
                        sent_mailbox_id = folders
                            .iter()
                            .find(|m| matches!(m.role, lookout_core::MailboxRole::Sent))
                            .map(|m| m.id.clone());
                    }
                    AccountEvent::SendCompleted => {
                        println!("SendCompleted received");
                        send_completed = true;
                        if let Some(sent) = sent_mailbox_id.clone() {
                            println!("re-syncing Sent mailbox to verify APPEND: {sent}");
                            let _ = cmd_tx.send(AccountCommand::SyncMailbox(sent)).await;
                        } else {
                            println!("no Sent mailbox identified; can't verify APPEND, but SMTP send succeeded");
                            break;
                        }
                    }
                    AccountEvent::MessagesUpdated { messages, .. } if send_completed => {
                        let found = messages.iter().any(|m| m.subject.as_deref() == Some(marker.as_str()));
                        if found {
                            println!("CONFIRMED: test message found in Sent via APPEND");
                        } else {
                            println!("WARNING: SendCompleted fired but test message not found in Sent (yet) - {} messages in view", messages.len());
                        }
                        break;
                    }
                    AccountEvent::MessagesUpdated { .. } => {}
                    AccountEvent::BodyFetched { .. } => {}
                    AccountEvent::MessageMoved { .. } | AccountEvent::MessageSnoozed => {}
                    AccountEvent::Error(e) => {
                        println!("ERROR: {e}");
                        break;
                    }
                }
            }
        }
    }

    let _ = cmd_tx.send(AccountCommand::Shutdown).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

    if send_completed {
        Ok(())
    } else {
        anyhow::bail!("send did not complete")
    }
}
