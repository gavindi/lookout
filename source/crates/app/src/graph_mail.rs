//! Reads and writes a message's Outlook properties on Microsoft 365 through
//! Microsoft Graph (item 5.4 / A-06, M-03, M-09, M-10, M-12; §4.4).
//!
//! Exchange's IMAP doesn't keep custom keywords and exposes none of Graph's
//! per-message properties (categories, importance, Focused/Other), so on
//! Microsoft 365 this module carries them to and from Outlook's own fields:
//! - **write**: a category, importance, flag or classification change is
//!   `PATCH`ed onto the Graph message resolved by `internetMessageId`;
//! - **read**: one `/me/messages/delta` page (cursor persisted in the cache)
//!   is handed to the session as `AccountCommand::ApplyRemoteMailProperties`,
//!   so a change made in Outlook shows here.
//!
//! A thin wrapper over `lookout_graph::MailClient` (item 5.4's transport), so
//! the wire mapping lives in one place; both directions only run once the
//! account has granted Graph access (Config's "Sync with Outlook").
//!
//! Due dates are date-only in Lookout (UTC midnight of the date), so a flag's
//! date is written as local midnight in the calendar's display zone and read
//! back the same way.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::sync::Arc;

use chrono::{DateTime, Utc};
use lookout_core::{AccountId, EmailAddress, FollowUpFlag, Importance, InferenceClassification};
use lookout_graph::{GraphClient, GraphError, InferenceOverride, MailClient, MessageDelta, OutgoingAttachment, OutgoingMessage};

use crate::microsoft_oauth::MicrosoftOAuth;

/// One Microsoft 365 account's Graph mail-property client.
pub struct GraphMailClient {
    oauth: Arc<MicrosoftOAuth>,
    mail: MailClient,
}

impl GraphMailClient {
    pub fn new(account_id: AccountId) -> Self {
        let oauth = Arc::new(MicrosoftOAuth::new(account_id));
        GraphMailClient {
            mail: MailClient::new(GraphClient::new(oauth.clone())),
            oauth,
        }
    }

    /// Whether the account has granted Graph access (disk-only check).
    pub fn consented(&self) -> bool {
        self.oauth.graph_consented()
    }

    /// Resolves an RFC 822 `Message-ID` to Graph's message id, or `None` when
    /// no message matches yet (it may not have landed in Graph's index).
    pub async fn resolve_message_id(&self, message_id: &str) -> Result<Option<String>, GraphError> {
        self.mail.resolve_message_id(message_id).await
    }

    /// One page (or incremental step) of the messages delta, split into live
    /// records and tombstones. `zone` interprets a flag's due date.
    pub async fn property_delta(&self, cursor: Option<&str>, zone: Option<chrono_tz::Tz>) -> Result<MessageDelta, GraphError> {
        self.mail.property_delta(cursor, zone).await
    }

    /// Sets or clears a message's follow-up flag (M-10).
    pub async fn set_follow_up(&self, graph_id: &str, flag: &FollowUpFlag, zone: Option<chrono_tz::Tz>) -> Result<(), GraphError> {
        self.mail.set_follow_up(graph_id, flag, zone).await
    }

    /// Replaces a message's categories with `names` (empty clears them, M-09).
    pub async fn set_categories(&self, graph_id: &str, names: &[String]) -> Result<(), GraphError> {
        self.mail.set_categories(graph_id, names).await
    }

    /// Sets a message's importance (M-12).
    pub async fn set_importance(&self, graph_id: &str, importance: Importance) -> Result<(), GraphError> {
        self.mail.set_importance(graph_id, importance).await
    }

    /// Sets a message's Focused / Other classification (M-03).
    pub async fn set_inference_classification(&self, graph_id: &str, classification: InferenceClassification) -> Result<(), GraphError> {
        self.mail.set_inference_classification(graph_id, classification).await
    }

    /// The account's Focused / Other sender overrides (M-03).
    pub async fn list_overrides(&self) -> Result<Vec<InferenceOverride>, GraphError> {
        self.mail.list_overrides().await
    }

    /// Creates or updates a sender override (Graph upserts by SMTP address),
    /// returning Graph's copy with its id.
    pub async fn create_override(&self, classification: InferenceClassification, sender: &EmailAddress) -> Result<Option<InferenceOverride>, GraphError> {
        self.mail.create_override(classification, sender).await
    }

    /// Deletes a sender override by id ("stop moving").
    pub async fn delete_override(&self, override_id: &str) -> Result<(), GraphError> {
        self.mail.delete_override(override_id).await
    }

    /// Sends `message` through Graph's `sendMail` with `PidTagDeferredSendTime`
    /// set to `at` (item 5.5 / C-08), so Exchange holds it in Drafts and sends
    /// it - even if Lookout has closed.
    pub async fn send_scheduled(&self, message: &lookout_mail::ComposedMessage, at: DateTime<Utc>) -> Result<(), GraphError> {
        self.mail.send_scheduled(&outgoing_message(message), Some(at)).await
    }
}

/// Maps a composed message to Graph's `sendMail` shape (item 5.5). `From` is
/// left to Graph (the signed-in mailbox), so a scheduled send goes out as the
/// account's own address - a sending identity is not carried here.
fn outgoing_message(message: &lookout_mail::ComposedMessage) -> OutgoingMessage {
    let recipients = |list: &[String]| list.iter().map(|address| EmailAddress::new(address.clone())).collect::<Vec<_>>();
    OutgoingMessage {
        subject: message.subject.clone(),
        text_body: message.text_body.clone(),
        html_body: message.html_body.clone(),
        importance: message.importance,
        to: recipients(&message.to),
        cc: recipients(&message.cc),
        bcc: recipients(&message.bcc),
        reply_to: message.reply_to.clone(),
        attachments: message
            .attachments
            .iter()
            .map(|attachment| OutgoingAttachment {
                name: attachment.filename.clone(),
                content_type: attachment.content_type.clone(),
                content_id: None,
                data: attachment.bytes.clone(),
            })
            .collect(),
        inline_images: message
            .inline_images
            .iter()
            .map(|image| OutgoingAttachment {
                name: inline_name(&image.cid),
                content_type: image.content_type.clone(),
                content_id: Some(image.cid.clone()),
                data: image.bytes.clone(),
            })
            .collect(),
    }
}

/// A display name for an inline image, from its `Content-ID` (Graph wants a
/// `name` for every attachment).
fn inline_name(cid: &str) -> String {
    let trimmed = cid.trim().trim_start_matches('<').trim_end_matches('>');
    if trimmed.is_empty() {
        "inline-image".to_string()
    } else {
        trimmed.to_string()
    }
}
