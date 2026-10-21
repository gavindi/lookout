//! The Microsoft Graph mail-property transport: the HTTP operations the mail
//! sync drives, layered on [`GraphClient`] (OUTLOOK_PARITY.md §4.4, item 5.4 /
//! A-06, M-03, M-09, M-10, M-12).
//!
//! Every method returns `lookout_core` types through the pure mapping in the
//! parent module, so the sync glue never touches Graph JSON. `@odata.nextLink`
//! paging is handled by [`GraphClient::get_all_with_headers`], and the messages
//! delta by [`GraphClient::delta_with_headers`], whose cursor the caller
//! persists.
//!
//! Every request carries `Prefer: IdType="ImmutableId"`: a message's default
//! Graph id *changes* when it moves between folders, which would churn the
//! cached Message-ID ↔ Graph-id map and make a delta's removed/added pair
//! ambiguous. The immutable id is stable for the message's lifetime.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use lookout_core::text::percent_encode;
use lookout_core::{EmailAddress, FollowUpFlag, Importance};
use serde_json::{json, Value};

use chrono::{DateTime, Utc};

use crate::client::{page_items, DeltaPage, GraphClient};
use crate::error::{GraphError, Result};

use super::message::{
    attachment_from_graph, comment_body, destination_body, draft_body, folder_name_body, mail_folder_from_graph, message_from_graph, search_param, GraphAttachment, GraphMessage,
    MailFolderDelta, MessagePatch, MessageSummaryDelta, WellKnownFolders, ATTACHMENT_SELECT, BODY_SELECT, FOLDER_SELECT, SUMMARY_SELECT, WELL_KNOWN_FOLDER_NAMES,
};
use super::{
    outgoing_message_body, override_body, override_from_graph, shared_mail_folder_from_graph, InferenceClassification, InferenceOverride, MessageDelta, MessageProperties,
    OutgoingMessage, SharedMailFolder,
};

/// `Prefer: IdType="ImmutableId"` - see the module notes.
const IMMUTABLE_ID: [(&str, &str); 1] = [("Prefer", "IdType=\"ImmutableId\"")];

/// [`IMMUTABLE_ID`] plus `Prefer: outlook.body-content-type="html"`, so a
/// message read returns its body as HTML (Graph converts a plain-text body)
/// rather than whatever form it was stored in.
const IMMUTABLE_ID_HTML_BODY: [(&str, &str); 2] = [("Prefer", "IdType=\"ImmutableId\""), ("Prefer", "outlook.body-content-type=\"html\"")];

/// The `$select` fields the property read consumes - exactly what the mapper
/// needs, so Graph doesn't return whole message bodies.
const PROPERTY_SELECT: &str = "$select=id,internetMessageId,categories,flag,importance,inferenceClassification";

/// Most sender overrides read for one account. Graph itself caps a mailbox at
/// 1000 overrides.
const MAX_OVERRIDES: usize = 1000;
/// Most folders read for one shared mailbox (item 5.6 / M-42).
const MAX_SHARED_FOLDERS: usize = 1000;
/// Most attachments listed for one message.
const MAX_ATTACHMENTS: usize = 250;
/// Most results one message search returns. Graph itself stops at 1000 for a
/// `$search`.
const MAX_SEARCH_RESULTS: usize = 1000;

/// The mail-property HTTP operations, over a shared [`GraphClient`].
pub struct MailClient {
    graph: GraphClient,
}

impl MailClient {
    pub fn new(graph: GraphClient) -> Self {
        MailClient { graph }
    }

    /// Resolves an RFC 822 `Message-ID` (angle brackets included, as the IMAP
    /// envelope carries it) to Graph's message id, or `None` when no message
    /// matches yet (it may not have landed in Graph's index).
    pub async fn resolve_message_id(&self, message_id: &str) -> Result<Option<String>> {
        let filter = percent_encode(&super::message_id_filter(message_id));
        let path = format!("me/messages?$filter={filter}&$select=id");
        let page = self.graph.get_with_headers(&path, &IMMUTABLE_ID).await?;
        Ok(page_items(&page).first().and_then(|message| message.get("id")).and_then(Value::as_str).map(str::to_string))
    }

    /// [`Self::resolve_message_id`] scoped to one folder - used to find the
    /// *draft* copy of a Message-ID without risking the sent copy Exchange
    /// filed in Sent Items after the same message went out.
    pub async fn resolve_message_id_in(&self, folder_id: &str, message_id: &str) -> Result<Option<String>> {
        let filter = percent_encode(&super::message_id_filter(message_id));
        let path = format!("me/mailFolders/{}/messages?$filter={filter}&$select=id", encode_segment(folder_id));
        let page = self.graph.get_with_headers(&path, &IMMUTABLE_ID).await?;
        Ok(page_items(&page).first().and_then(|message| message.get("id")).and_then(Value::as_str).map(str::to_string))
    }

    /// Saves raw RFC 5322 bytes as a new draft (`POST /me/messages` with
    /// `Content-Type: text/rfc822`), returning it with its id. MIME is the only
    /// form that preserves the compose session's stable `Message-ID` and Bcc
    /// header, which repeated autosaves use to replace rather than duplicate
    /// (MAIL_GRAPH_OVERVIEW.md item 5.7.5). [`Self::create_draft`] remains the
    /// JSON form for callers that only have an [`OutgoingMessage`].
    pub async fn create_draft_mime(&self, raw: &[u8]) -> Result<Option<GraphMessage>> {
        let created = self.graph.post_raw("me/messages", raw, "text/rfc822").await?;
        Ok(message_from_graph(&created, None))
    }

    /// Reads one message's synced properties by Graph id, or `None` when it no
    /// longer exists. `zone` interprets a follow-up flag's due date.
    pub async fn get_properties(&self, graph_id: &str, zone: Option<chrono_tz::Tz>) -> Result<Option<MessageProperties>> {
        let path = format!("me/messages/{}?{PROPERTY_SELECT}", encode_segment(graph_id));
        match self.graph.get_with_headers(&path, &IMMUTABLE_ID).await {
            Ok(value) => Ok(super::message_properties_from_graph(&value, zone)),
            Err(GraphError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// One page (or incremental step) of the messages delta, split into live
    /// records and tombstones. Pass `None` for the initial read, or a
    /// previously returned [`MessageDelta::delta_link`] for the next one.
    pub async fn property_delta(&self, cursor: Option<&str>, zone: Option<chrono_tz::Tz>) -> Result<MessageDelta> {
        let path = format!("me/messages/delta?{PROPERTY_SELECT}&$top=100");
        let page = self.graph.delta_with_headers(&path, cursor, &IMMUTABLE_ID).await?;
        Ok(delta_from_page(page, zone))
    }

    /// One step of a **folder-scoped** messages delta
    /// (`GET /me/mailFolders/{folder_id}/messages/delta`), the per-folder read
    /// the 5.7 spike measures and a Graph mail transport would drive
    /// (MAIL_GRAPH_OVERVIEW.md items 5.7.1/5.7.2). `cursor` is a previously
    /// returned [`MessageDelta::delta_link`]; `None` starts the initial read.
    pub async fn folder_delta(&self, folder_id: &str, cursor: Option<&str>, zone: Option<chrono_tz::Tz>) -> Result<MessageDelta> {
        let path = format!("me/mailFolders/{}/messages/delta?{PROPERTY_SELECT}&$top=100", encode_segment(folder_id));
        let page = self.graph.delta_with_headers(&path, cursor, &IMMUTABLE_ID).await?;
        Ok(delta_from_page(page, zone))
    }

    /// The raw MIME (`message/rfc822`) bytes of one message
    /// (`GET /me/messages/{id}/$value`), or `None` when it no longer exists.
    /// Carries `Prefer: IdType="ImmutableId"` for the same reason every other
    /// message call does (see the module notes) - and because Graph rejects a
    /// `/$value` read against a mutable id once the message has moved. This is
    /// the read MAIL_GRAPH_OVERVIEW.md item 5.7.1 gates the deferred Graph mail
    /// transport on: whether it reproduces the IMAP body byte for byte.
    pub async fn get_mime(&self, graph_id: &str) -> Result<Option<Vec<u8>>> {
        self.graph
            .get_bytes_with_headers(&format!("me/messages/{}/$value", encode_segment(graph_id)), &IMMUTABLE_ID)
            .await
    }

    /// Applies a partial `PATCH` to one message (categories, importance,
    /// classification and/or the flag in one request), returning Graph's copy.
    pub async fn patch_message(&self, graph_id: &str, body: &Value) -> Result<Value> {
        self.graph
            .patch_with_headers(&format!("me/messages/{}", encode_segment(graph_id)), body, &IMMUTABLE_ID)
            .await
    }

    /// Replaces a message's categories with `categories` (empty clears them).
    pub async fn set_categories(&self, graph_id: &str, categories: &[String]) -> Result<()> {
        self.patch_message(graph_id, &super::categories_body(categories)).await.map(|_| ())
    }

    /// Sets a message's importance (M-12).
    pub async fn set_importance(&self, graph_id: &str, importance: Importance) -> Result<()> {
        self.patch_message(graph_id, &super::importance_body(importance)).await.map(|_| ())
    }

    /// Sets a message's Focused / Other classification (M-03).
    pub async fn set_inference_classification(&self, graph_id: &str, classification: InferenceClassification) -> Result<()> {
        self.patch_message(graph_id, &super::classification_body(classification)).await.map(|_| ())
    }

    /// Sets or clears a message's follow-up flag (M-10).
    pub async fn set_follow_up(&self, graph_id: &str, flag: &FollowUpFlag, zone: Option<chrono_tz::Tz>) -> Result<()> {
        self.patch_message(graph_id, &json!({ "flag": super::flag_body(flag, zone) })).await.map(|_| ())
    }

    /// Sends `message` through Graph's `sendMail` (item 5.5 / C-08). With
    /// `deferred_send_at`, the request carries the `PidTagDeferredSendTime`
    /// extended property (`SystemTime 0x3FEF`), so Exchange holds the message
    /// in Drafts until then and sends it without Lookout's involvement.
    pub async fn send_scheduled(&self, message: &OutgoingMessage, deferred_send_at: Option<DateTime<Utc>>) -> Result<()> {
        let body = json!({ "message": outgoing_message_body(message, deferred_send_at) });
        self.graph.post("me/sendMail", &body).await.map(|_| ())
    }

    /// Sends an already-built RFC 5322 message through Graph's MIME form of
    /// `sendMail` (`Content-Type: text/rfc822`). The local outbox (item 0.3)
    /// stores the exact bytes the mail crate built, so draining it over Graph
    /// must not re-render them through the JSON [`OutgoingMessage`] shape -
    /// MIME is the only form that preserves attachments, an iMIP payload and a
    /// read-receipt report faithfully (MAIL_GRAPH_OVERVIEW.md item 5.7.5).
    pub async fn send_mime(&self, raw: &[u8]) -> Result<()> {
        self.graph.post_raw("me/sendMail", raw, "text/rfc822").await.map(|_| ())
    }

    /// Sends `message` **as** a shared mailbox (`owner`, a Graph user id or
    /// address) through `POST /users/{owner}/sendMail` (item 5.6 / M-42).
    /// Needs the `Mail.Send.Shared` scope (`GraphFeature::SharedMailboxes`).
    pub async fn send_mail_as(&self, owner: &str, message: &OutgoingMessage, deferred_send_at: Option<DateTime<Utc>>) -> Result<()> {
        let body = json!({ "message": outgoing_message_body(message, deferred_send_at) });
        self.graph.post(&format!("users/{}/sendMail", encode_segment(owner)), &body).await.map(|_| ())
    }

    /// A shared mailbox's folders (`GET /users/{owner}/mailFolders`,
    /// item 5.6 / M-42). Needs the `Mail.ReadWrite.Shared` scope
    /// (`GraphFeature::SharedMailboxes`).
    pub async fn list_mail_folders(&self, owner: &str) -> Result<Vec<SharedMailFolder>> {
        let path = format!(
            "users/{}/mailFolders?$top=100&$select=id,displayName,parentFolderId,totalItemCount,unreadItemCount",
            encode_segment(owner)
        );
        let items = self.graph.get_all(&path, MAX_SHARED_FOLDERS).await?;
        Ok(items.iter().filter_map(shared_mail_folder_from_graph).collect())
    }

    /// The account's Focused / Other sender overrides (M-03).
    pub async fn list_overrides(&self) -> Result<Vec<InferenceOverride>> {
        let items = self
            .graph
            .get_all("me/inferenceClassification/overrides?$top=100&$select=id,classifyAs,senderEmailAddress", MAX_OVERRIDES)
            .await?;
        Ok(items.iter().filter_map(override_from_graph).collect())
    }

    /// Creates or updates a sender override (Graph upserts by SMTP address),
    /// returning Graph's copy with its id.
    pub async fn create_override(&self, classify_as: InferenceClassification, sender: &EmailAddress) -> Result<Option<InferenceOverride>> {
        let created = self.graph.post("me/inferenceClassification/overrides", &override_body(classify_as, sender)).await?;
        Ok(override_from_graph(&created))
    }

    /// Deletes a sender override by id ("stop moving").
    pub async fn delete_override(&self, override_id: &str) -> Result<()> {
        self.graph
            .delete(&format!("me/inferenceClassification/overrides/{}", encode_segment(override_id)))
            .await
            .map(|_| ())
    }

    // --- The Graph mail transport (MAIL_GRAPH_OVERVIEW.md item 5.7.2) ---
    //
    // The own-mailbox operations a Graph mail session would drive in place of
    // IMAP. Nothing calls them yet: the session (item 5.7.5) and the identity
    // model that turns Graph ids into `Uid`s (item 5.7.3) come later.

    /// One read of the mailbox's folder delta (`GET /me/mailFolders/delta`):
    /// the whole folder hierarchy on the first read (`cursor` = `None`), then
    /// only what changed. The plain `/me/mailFolders` list returns top-level
    /// folders only, which is why the delta is used even for the first read.
    pub async fn own_mail_folders_delta(&self, cursor: Option<&str>) -> Result<MailFolderDelta> {
        let page = self.graph.delta_with_headers(&format!("me/mailFolders/delta?{FOLDER_SELECT}"), cursor, &[]).await?;
        let mut delta = MailFolderDelta {
            delta_link: page.delta_link,
            ..Default::default()
        };
        for item in &page.items {
            if let Some(id) = super::removed_id(item) {
                delta.removals.push(id);
            } else if let Some(folder) = mail_folder_from_graph(item) {
                delta.folders.push(folder);
            }
        }
        Ok(delta)
    }

    /// Resolves Graph's well-known folder names to this mailbox's folder ids,
    /// one `GET /me/mailFolders/{name}` each. A folder the mailbox doesn't
    /// have (Graph answers 404, typically for `archive`) is `None`.
    pub async fn well_known_folders(&self) -> Result<WellKnownFolders> {
        let mut folders = WellKnownFolders::default();
        for name in WELL_KNOWN_FOLDER_NAMES {
            let id = match self.graph.get(&format!("me/mailFolders/{name}?$select=id")).await {
                Ok(value) => value.get("id").and_then(Value::as_str).map(str::to_string),
                Err(GraphError::Status { status: 404, .. }) => None,
                Err(e) => return Err(e),
            };
            folders.set(name, id);
        }
        Ok(folders)
    }

    /// One read of a folder's messages delta at list-row weight
    /// (`GET /me/mailFolders/{id}/messages/delta`). `cursor` is a previously
    /// returned [`MessageSummaryDelta::delta_link`]; `None` starts the initial
    /// read. An expired cursor fails with an error whose
    /// [`GraphError::is_delta_reset`] is true. Unlike [`Self::folder_delta`],
    /// which reads properties only, each record maps to a whole row.
    pub async fn folder_messages_delta(&self, folder_id: &str, cursor: Option<&str>, zone: Option<chrono_tz::Tz>) -> Result<MessageSummaryDelta> {
        let path = format!("me/mailFolders/{}/messages/delta?{SUMMARY_SELECT}", encode_segment(folder_id));
        let page = self.graph.delta_with_headers(&path, cursor, &IMMUTABLE_ID).await?;
        let mut delta = MessageSummaryDelta {
            delta_link: page.delta_link,
            ..Default::default()
        };
        for item in &page.items {
            if let Some(id) = super::removed_id(item) {
                delta.removals.push(id);
            } else if let Some(message) = message_from_graph(item, zone) {
                delta.upserts.push(message);
            }
        }
        Ok(delta)
    }

    /// One message at list-row weight (`GET /me/messages/{id}`), or `None` when
    /// it no longer exists. Used after a copy, whose `copy` response carries
    /// only the new id, so the target folder gets a complete row.
    pub async fn get_summary(&self, graph_id: &str, zone: Option<chrono_tz::Tz>) -> Result<Option<GraphMessage>> {
        let path = format!("me/messages/{}?{SUMMARY_SELECT}", encode_segment(graph_id));
        match self.graph.get_with_headers(&path, &IMMUTABLE_ID).await {
            Ok(value) => Ok(message_from_graph(&value, zone)),
            Err(GraphError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// One message with its body and headers (`GET /me/messages/{id}`): the
    /// row record plus the raw JSON that [`super::email_body_from_graph`]
    /// maps, or `None` when the message no longer exists. The body comes back
    /// as HTML.
    pub async fn get_message(&self, graph_id: &str, zone: Option<chrono_tz::Tz>) -> Result<Option<(GraphMessage, Value)>> {
        let path = format!("me/messages/{}?{SUMMARY_SELECT},{BODY_SELECT}", encode_segment(graph_id));
        match self.graph.get_with_headers(&path, &IMMUTABLE_ID_HTML_BODY).await {
            Ok(value) => Ok(message_from_graph(&value, zone).map(|message| (message, value))),
            Err(GraphError::Status { status: 404, .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Moves a message to `destination` (a folder id or well-known name),
    /// returning it as it now stands. With immutable ids the message keeps its
    /// id across the move.
    pub async fn move_message(&self, graph_id: &str, destination: &str) -> Result<Option<GraphMessage>> {
        let path = format!("me/messages/{}/move", encode_segment(graph_id));
        let moved = self.graph.post_with_headers(&path, &destination_body(destination), &IMMUTABLE_ID).await?;
        Ok(message_from_graph(&moved, None))
    }

    /// Copies a message to `destination`, returning the copy, which has its
    /// own new id.
    pub async fn copy_message(&self, graph_id: &str, destination: &str) -> Result<Option<GraphMessage>> {
        let path = format!("me/messages/{}/copy", encode_segment(graph_id));
        let copy = self.graph.post_with_headers(&path, &destination_body(destination), &IMMUTABLE_ID).await?;
        Ok(message_from_graph(&copy, None))
    }

    /// Deletes a message (`DELETE /me/messages/{id}`). Like an IMAP expunge it
    /// skips Deleted Items, which Exchange keeps as recoverable items; moving
    /// to Trash is [`Self::move_message`] to `deleteditems`.
    pub async fn delete_message(&self, graph_id: &str) -> Result<()> {
        self.graph
            .delete_with_headers(&format!("me/messages/{}", encode_segment(graph_id)), &IMMUTABLE_ID)
            .await
            .map(|_| ())
    }

    /// Writes a [`MessagePatch`] (read state, flag, categories, importance,
    /// Focused / Other) in one request. An empty patch sends nothing.
    pub async fn update_message(&self, graph_id: &str, patch: &MessagePatch, zone: Option<chrono_tz::Tz>) -> Result<()> {
        if patch.is_empty() {
            return Ok(());
        }
        self.patch_message(graph_id, &patch.body(zone)).await.map(|_| ())
    }

    /// A message's attachments, metadata only
    /// (`GET /me/messages/{id}/attachments`).
    pub async fn list_attachments(&self, graph_id: &str) -> Result<Vec<GraphAttachment>> {
        let path = format!("me/messages/{}/attachments?{ATTACHMENT_SELECT}", encode_segment(graph_id));
        let items = self.graph.get_all_with_headers(&path, MAX_ATTACHMENTS, &IMMUTABLE_ID).await?;
        Ok(items.iter().filter_map(attachment_from_graph).collect())
    }

    /// One attachment's raw bytes
    /// (`GET /me/messages/{id}/attachments/{attachment_id}/$value`), or `None`
    /// when it no longer exists. Takes the raw Graph attachment id; a cached
    /// `BodyPart::part_number` decodes to it with
    /// [`super::attachment_id_from_part`].
    pub async fn get_attachment_bytes(&self, graph_id: &str, attachment_id: &str) -> Result<Option<Vec<u8>>> {
        let path = format!("me/messages/{}/attachments/{}/$value", encode_segment(graph_id), encode_segment(attachment_id));
        self.graph.get_bytes_with_headers(&path, &IMMUTABLE_ID).await
    }

    /// Saves `message` as a new draft in Drafts (`POST /me/messages`),
    /// returning it with its id.
    pub async fn create_draft(&self, message: &OutgoingMessage) -> Result<Option<GraphMessage>> {
        let created = self.graph.post_with_headers("me/messages", &draft_body(message), &IMMUTABLE_ID).await?;
        Ok(message_from_graph(&created, None))
    }

    /// Rewrites an existing draft with `message`. Graph replaces the fields
    /// sent, so attachments already on the draft stay unless removed
    /// separately.
    pub async fn update_draft(&self, graph_id: &str, message: &OutgoingMessage) -> Result<()> {
        self.patch_message(graph_id, &draft_body(message)).await.map(|_| ())
    }

    /// Creates a reply draft (`createReply`) with `comment` above the quoted
    /// original, returning the draft.
    pub async fn create_reply(&self, graph_id: &str, comment: &str) -> Result<Option<GraphMessage>> {
        self.create_response(graph_id, "createReply", comment).await
    }

    /// Creates a reply-all draft (`createReplyAll`).
    pub async fn create_reply_all(&self, graph_id: &str, comment: &str) -> Result<Option<GraphMessage>> {
        self.create_response(graph_id, "createReplyAll", comment).await
    }

    /// Creates a forward draft (`createForward`), recipients still empty.
    pub async fn create_forward(&self, graph_id: &str, comment: &str) -> Result<Option<GraphMessage>> {
        self.create_response(graph_id, "createForward", comment).await
    }

    async fn create_response(&self, graph_id: &str, action: &str, comment: &str) -> Result<Option<GraphMessage>> {
        let path = format!("me/messages/{}/{action}", encode_segment(graph_id));
        let draft = self.graph.post_with_headers(&path, &comment_body(comment), &IMMUTABLE_ID).await?;
        Ok(message_from_graph(&draft, None))
    }

    /// Sends an existing draft (`POST /me/messages/{id}/send`); Exchange files
    /// the sent copy in Sent Items.
    pub async fn send_draft(&self, graph_id: &str) -> Result<()> {
        let path = format!("me/messages/{}/send", encode_segment(graph_id));
        self.graph.post_empty_with_headers(&path, &IMMUTABLE_ID).await.map(|_| ())
    }

    /// Creates a folder named `name`, top level when `parent_id` is `None`,
    /// returning it.
    pub async fn create_folder(&self, parent_id: Option<&str>, name: &str) -> Result<Option<SharedMailFolder>> {
        let path = match parent_id {
            Some(parent) => format!("me/mailFolders/{}/childFolders", encode_segment(parent)),
            None => "me/mailFolders".to_string(),
        };
        let created = self.graph.post(&path, &folder_name_body(name)).await?;
        Ok(mail_folder_from_graph(&created))
    }

    /// Renames a folder.
    pub async fn rename_folder(&self, folder_id: &str, name: &str) -> Result<()> {
        self.graph
            .patch(&format!("me/mailFolders/{}", encode_segment(folder_id)), &folder_name_body(name))
            .await
            .map(|_| ())
    }

    /// Deletes a folder and everything in it.
    pub async fn delete_folder(&self, folder_id: &str) -> Result<()> {
        self.graph.delete(&format!("me/mailFolders/{}", encode_segment(folder_id))).await.map(|_| ())
    }

    /// A free-text search of the mailbox, or of one folder, through Graph's
    /// `$search`, capped at `limit` results. Graph ranks the results itself and
    /// doesn't allow `$orderby` with `$search`, so callers sort.
    pub async fn search_messages(&self, folder_id: Option<&str>, text: &str, limit: usize, zone: Option<chrono_tz::Tz>) -> Result<Vec<GraphMessage>> {
        let scope = match folder_id {
            Some(folder) => format!("me/mailFolders/{}/messages", encode_segment(folder)),
            None => "me/messages".to_string(),
        };
        let path = format!("{scope}?{}&{SUMMARY_SELECT}&$top=50", search_param(text));
        let items = self.graph.get_all_with_headers(&path, limit.min(MAX_SEARCH_RESULTS), &IMMUTABLE_ID).await?;
        Ok(items.iter().filter_map(|item| message_from_graph(item, zone)).collect())
    }
}

/// Splits one delta read into live records and tombstones, carrying the fresh
/// cursor. Shared by the mailbox-wide and folder-scoped deltas.
fn delta_from_page(page: DeltaPage, zone: Option<chrono_tz::Tz>) -> MessageDelta {
    let mut delta = MessageDelta {
        delta_link: page.delta_link,
        ..Default::default()
    };
    for item in &page.items {
        if let Some(id) = super::removed_id(item) {
            delta.removals.push(id);
        } else if let Some(properties) = super::message_properties_from_graph(item, zone) {
            delta.upserts.push(properties);
        }
    }
    delta
}

/// Percent-encodes a path segment (a Graph message/override id), leaving RFC
/// 3986 unreserved characters. Graph ids are base64-ish and may contain `+`,
/// `/` or `=`, which must not leak into the URL path.
fn encode_segment(segment: &str) -> String {
    percent_encode(segment)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use wiremock::matchers::{body_json, body_partial_json, header, headers, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::auth::TokenProvider;
    use lookout_core::FlagStatus;

    struct StubTokens;

    #[async_trait]
    impl TokenProvider for StubTokens {
        async fn graph_token(&self) -> Result<String> {
            Ok("stub".to_string())
        }
        async fn ensure_scopes(&self, _scopes: &[&str]) -> Result<()> {
            Ok(())
        }
    }

    fn client(server: &MockServer) -> MailClient {
        MailClient::new(GraphClient::with_base_url(Arc::new(StubTokens), format!("{}/v1.0", server.uri())))
    }

    fn message_json() -> Value {
        json!({
            "id": "msg-1",
            "internetMessageId": "<abc@contoso.com>",
            "categories": ["Work"],
            "importance": "high",
            "inferenceClassification": "other"
        })
    }

    #[tokio::test]
    async fn resolve_message_id_sends_the_filter_and_immutable_id_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .and(query_param("$filter", "internetMessageId eq '<abc@contoso.com>'"))
            .and(query_param("$select", "id"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{ "id": "msg-1" }] })))
            .mount(&server)
            .await;

        assert_eq!(client(&server).resolve_message_id("<abc@contoso.com>").await.unwrap().as_deref(), Some("msg-1"));
    }

    #[tokio::test]
    async fn resolve_message_id_is_none_when_graph_has_no_match() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
            .mount(&server)
            .await;
        assert_eq!(client(&server).resolve_message_id("<missing@x>").await.unwrap(), None);
    }

    #[tokio::test]
    async fn get_properties_selects_the_fields_and_maps_them() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/msg-1"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(message_json()))
            .mount(&server)
            .await;

        let props = client(&server).get_properties("msg-1", None).await.unwrap().unwrap();
        assert_eq!(props.graph_id, "msg-1");
        assert_eq!(props.categories, vec!["Work".to_string()]);
        assert_eq!(props.importance, Importance::High);
        assert_eq!(props.classification, InferenceClassification::Other);
    }

    #[tokio::test]
    async fn get_properties_is_none_on_not_found() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
        assert!(client(&server).get_properties("gone", None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn property_delta_splits_upserts_from_removals_and_returns_the_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/delta"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [message_json(), { "id": "gone", "@removed": { "reason": "deleted" } }],
                "@odata.deltaLink": format!("{}/v1.0/delta-cursor", server.uri())
            })))
            .mount(&server)
            .await;

        let delta = client(&server).property_delta(None, None).await.unwrap();
        assert_eq!(delta.upserts.len(), 1);
        assert_eq!(delta.upserts[0].graph_id, "msg-1");
        assert_eq!(delta.removals, vec!["gone".to_string()]);
        assert_eq!(delta.delta_link.as_deref(), Some(format!("{}/v1.0/delta-cursor", server.uri()).as_str()));
    }

    #[tokio::test]
    async fn folder_delta_is_scoped_to_the_folder_and_maps_the_page() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/inbox/messages/delta"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [message_json(), { "id": "gone", "@removed": { "reason": "deleted" } }],
                "@odata.deltaLink": format!("{}/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=abc", server.uri())
            })))
            .mount(&server)
            .await;

        let delta = client(&server).folder_delta("inbox", None, None).await.unwrap();
        assert_eq!(delta.upserts.len(), 1);
        assert_eq!(delta.upserts[0].graph_id, "msg-1");
        assert_eq!(delta.upserts[0].categories, vec!["Work".to_string()]);
        assert_eq!(delta.removals, vec!["gone".to_string()]);
        assert_eq!(
            delta.delta_link.as_deref(),
            Some(format!("{}/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=abc", server.uri()).as_str())
        );
    }

    #[tokio::test]
    async fn folder_delta_follows_a_supplied_cursor() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/inbox/messages/delta"))
            .and(query_param("$deltatoken", "abc"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [] })))
            .mount(&server)
            .await;

        let delta = client(&server)
            .folder_delta("inbox", Some(&format!("{}/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=abc", server.uri())), None)
            .await
            .unwrap();
        assert!(delta.upserts.is_empty());
        assert!(delta.removals.is_empty());
        assert_eq!(delta.delta_link, None, "a page with neither cursor returns no delta link");
    }

    #[tokio::test]
    async fn get_mime_returns_the_raw_bytes_and_sends_the_immutable_id_header() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/msg-1/$value"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"MIME-Version: 1.0\r\n\r\nbody".to_vec()))
            .mount(&server)
            .await;

        let mime = client(&server).get_mime("msg-1").await.unwrap();
        assert_eq!(mime.as_deref(), Some(b"MIME-Version: 1.0\r\n\r\nbody".as_slice()));
    }

    #[tokio::test]
    async fn get_mime_is_none_when_the_message_is_gone() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/msg-1/$value"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        assert!(client(&server).get_mime("msg-1").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn the_write_helpers_send_their_bodies() {
        let server = MockServer::start().await;
        for (suffix, body) in [
            ("msg-1", json!({ "categories": ["Work", "Home"] })),
            ("msg-2", json!({ "importance": "low" })),
            ("msg-3", json!({ "inferenceClassification": "focused" })),
        ] {
            Mock::given(method("PATCH"))
                .and(path(format!("/v1.0/me/messages/{suffix}")))
                .and(header("Prefer", "IdType=\"ImmutableId\""))
                .and(body_partial_json(body.clone()))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": suffix })))
                .mount(&server)
                .await;
        }

        let client = client(&server);
        client.set_categories("msg-1", &["Work".to_string(), "Home".to_string()]).await.unwrap();
        client.set_importance("msg-2", Importance::Low).await.unwrap();
        client.set_inference_classification("msg-3", InferenceClassification::Focused).await.unwrap();
    }

    #[tokio::test]
    async fn set_follow_up_patches_the_flag_body() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/messages/msg-1"))
            .and(body_partial_json(json!({ "flag": { "flagStatus": "complete" } })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "msg-1" })))
            .mount(&server)
            .await;

        let flag = FollowUpFlag {
            status: FlagStatus::Complete,
            ..Default::default()
        };
        client(&server).set_follow_up("msg-1", &flag, None).await.unwrap();
    }

    #[tokio::test]
    async fn overrides_list_create_and_delete() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/inferenceClassification/overrides"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "o1", "classifyAs": "focused", "senderEmailAddress": { "name": "Sam", "address": "sam@contoso.com" } }]
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/inferenceClassification/overrides"))
            .and(body_partial_json(json!({ "classifyAs": "other", "senderEmailAddress": { "address": "sam@contoso.com" } })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "o2", "classifyAs": "other", "senderEmailAddress": { "address": "sam@contoso.com" }
            })))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/inferenceClassification/overrides/o1"))
            .respond_with(ResponseTemplate::new(204))
            .mount(&server)
            .await;

        let client = client(&server);
        let overrides = client.list_overrides().await.unwrap();
        assert_eq!(overrides.len(), 1);
        assert_eq!(overrides[0].classify_as, InferenceClassification::Focused);

        let sender = EmailAddress::new("sam@contoso.com");
        let created = client.create_override(InferenceClassification::Other, &sender).await.unwrap().unwrap();
        assert_eq!(created.id, "o2");

        client.delete_override("o1").await.unwrap();
    }

    #[tokio::test]
    async fn send_scheduled_posts_send_mail_with_the_deferred_property() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/sendMail"))
            .and(body_partial_json(json!({
                "message": {
                    "subject": "Later",
                    "importance": "normal",
                    "singleValueExtendedProperties": [{ "id": "SystemTime 0x3FEF", "value": "2026-10-17T08:00:00Z" }]
                }
            })))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let message = OutgoingMessage {
            subject: "Later".to_string(),
            text_body: "hi".to_string(),
            html_body: None,
            importance: Importance::Normal,
            to: vec![EmailAddress::new("a@b.example")],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            attachments: Vec::new(),
            inline_images: Vec::new(),
        };
        let at = "2026-10-17T08:00:00Z".parse::<DateTime<Utc>>().unwrap();
        client(&server).send_scheduled(&message, Some(at)).await.unwrap();
    }

    #[tokio::test]
    async fn list_mail_folders_reads_a_shared_mailbox() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/users/support%40example.com/mailFolders"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "value": [{ "id": "inbox", "displayName": "Inbox", "totalItemCount": 5, "unreadItemCount": 2 }]
            })))
            .mount(&server)
            .await;

        let folders = client(&server).list_mail_folders("support@example.com").await.unwrap();
        assert_eq!(folders.len(), 1);
        assert_eq!(folders[0].display_name, "Inbox");
        assert_eq!(folders[0].unread, 2);
    }

    #[tokio::test]
    async fn send_mail_as_posts_under_the_shared_mailbox() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/users/support%40example.com/sendMail"))
            .and(body_partial_json(json!({ "message": { "subject": "From support" } })))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        let message = OutgoingMessage {
            subject: "From support".to_string(),
            text_body: "hi".to_string(),
            html_body: None,
            importance: Importance::Normal,
            to: vec![EmailAddress::new("a@b.example")],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            attachments: Vec::new(),
            inline_images: Vec::new(),
        };
        client(&server).send_mail_as("support@example.com", &message, None).await.unwrap();
    }

    #[tokio::test]
    async fn resolve_message_id_in_scopes_the_filter_to_a_folder() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/drafts-id/messages"))
            .and(query_param("$filter", "internetMessageId eq '<abc@contoso.com>'"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [{ "id": "draft-1" }] })))
            .mount(&server)
            .await;

        assert_eq!(
            client(&server).resolve_message_id_in("drafts-id", "<abc@contoso.com>").await.unwrap().as_deref(),
            Some("draft-1")
        );
    }

    #[tokio::test]
    async fn a_mime_draft_is_posted_to_the_messages_endpoint() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/messages"))
            .and(header("Content-Type", "text/rfc822"))
            .and(wiremock::matchers::body_string("Message-ID: <d@lookout.local>\r\n\r\nhi"))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({
                "id": "d1", "isDraft": true, "internetMessageId": "<d@lookout.local>"
            })))
            .mount(&server)
            .await;

        let draft = client(&server).create_draft_mime(b"Message-ID: <d@lookout.local>\r\n\r\nhi").await.unwrap().unwrap();
        assert_eq!(draft.graph_id, "d1");
        assert!(draft.is_draft);
        assert_eq!(draft.message_id.as_deref(), Some("<d@lookout.local>"));
    }

    #[tokio::test]
    async fn send_mime_posts_the_raw_bytes_as_rfc822() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/sendMail"))
            .and(header("Content-Type", "text/rfc822"))
            .and(wiremock::matchers::body_string("Subject: raw\r\n\r\nbody"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;

        client(&server).send_mime(b"Subject: raw\r\n\r\nbody").await.unwrap();
    }

    #[tokio::test]
    async fn a_failed_write_maps_to_a_status_error() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/messages/msg-1"))
            .respond_with(ResponseTemplate::new(403).set_body_string("forbidden"))
            .mount(&server)
            .await;
        match client(&server).set_importance("msg-1", Importance::High).await.unwrap_err() {
            GraphError::Status { status, .. } => assert_eq!(status, 403),
            other => panic!("expected a status error, got {other:?}"),
        }
    }

    // --- The Graph mail transport (item 5.7.2) ---

    fn summary_json(id: &str) -> Value {
        json!({
            "id": id,
            "parentFolderId": "inbox-id",
            "conversationId": "conv-1",
            "subject": "Hello",
            "from": { "emailAddress": { "name": "Ada", "address": "ada@contoso.com" } },
            "receivedDateTime": "2026-10-14T09:30:00Z",
            "isRead": false
        })
    }

    fn outgoing(subject: &str) -> OutgoingMessage {
        OutgoingMessage {
            subject: subject.to_string(),
            text_body: "hi".to_string(),
            html_body: None,
            importance: Importance::Normal,
            to: vec![EmailAddress::new("a@b.example")],
            cc: Vec::new(),
            bcc: Vec::new(),
            reply_to: Vec::new(),
            attachments: Vec::new(),
            inline_images: Vec::new(),
        }
    }

    #[tokio::test]
    async fn own_mail_folders_delta_reads_the_whole_hierarchy() {
        let server = MockServer::start().await;
        let mut page = serde_json::from_str::<Value>(include_str!("../../../../test-fixtures/outlook/mail-folders-delta.json")).unwrap();
        page["@odata.deltaLink"] = json!(format!("{}/v1.0/me/mailFolders/delta?$deltatoken=folders-1", server.uri()));
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/delta"))
            .and(query_param("$select", "id,displayName,parentFolderId,childFolderCount,totalItemCount,unreadItemCount"))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .mount(&server)
            .await;

        let delta = client(&server).own_mail_folders_delta(None).await.unwrap();
        assert_eq!(delta.folders.len(), 4);
        assert_eq!(delta.folders[1].display_name, "Projects");
        assert_eq!(delta.removals, vec!["AAMkAGOLDFOLDER=".to_string()]);
        assert!(delta.delta_link.unwrap().ends_with("$deltatoken=folders-1"));
    }

    #[tokio::test]
    async fn well_known_folders_resolves_each_name_and_tolerates_a_missing_archive() {
        let server = MockServer::start().await;
        for name in ["inbox", "sentitems", "drafts", "deleteditems", "junkemail"] {
            Mock::given(method("GET"))
                .and(path(format!("/v1.0/me/mailFolders/{name}")))
                .and(query_param("$select", "id"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": format!("{name}-id") })))
                .mount(&server)
                .await;
        }
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/archive"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let folders = client(&server).well_known_folders().await.unwrap();
        assert_eq!(folders.inbox.as_deref(), Some("inbox-id"));
        assert_eq!(folders.deleted.as_deref(), Some("deleteditems-id"));
        assert_eq!(folders.junk.as_deref(), Some("junkemail-id"));
        assert_eq!(folders.archive, None);
    }

    #[tokio::test]
    async fn folder_messages_delta_maps_whole_rows() {
        let server = MockServer::start().await;
        let mut page = serde_json::from_str::<Value>(include_str!("../../../../test-fixtures/outlook/message-summary-delta.json")).unwrap();
        page["@odata.deltaLink"] = json!(format!("{}/v1.0/cursor", server.uri()));
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/AAMkAGINBOX%3D/messages/delta"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(page))
            .mount(&server)
            .await;

        let delta = client(&server).folder_messages_delta("AAMkAGINBOX=", None, None).await.unwrap();
        assert_eq!(delta.upserts.len(), 4);
        assert_eq!(delta.upserts[0].subject.as_deref(), Some("Quarterly report"));
        assert!(delta.upserts[3].is_meeting);
        assert_eq!(delta.removals, vec!["AAMkAGGONE=".to_string()]);
        assert_eq!(delta.delta_link, Some(format!("{}/v1.0/cursor", server.uri())));
    }

    #[tokio::test]
    async fn an_expired_delta_cursor_reads_as_a_reset() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/inbox/messages/delta"))
            .respond_with(ResponseTemplate::new(410).set_body_string(r#"{"error":{"code":"syncStateNotFound"}}"#))
            .mount(&server)
            .await;

        let error = client(&server)
            .folder_messages_delta("inbox", Some(&format!("{}/v1.0/me/mailFolders/inbox/messages/delta?$deltatoken=old", server.uri())), None)
            .await
            .unwrap_err();
        assert!(error.is_delta_reset());
        assert!(!GraphError::Unauthorized.is_delta_reset());
    }

    #[tokio::test]
    async fn get_message_asks_for_an_html_body_and_returns_the_raw_json() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/AAMkAGMSG1%3D"))
            .and(headers("Prefer", vec!["IdType=\"ImmutableId\"", "outlook.body-content-type=\"html\""]))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::from_str::<Value>(include_str!("../../../../test-fixtures/outlook/message-full.json")).unwrap()))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = client(&server);
        let (message, raw) = client.get_message("AAMkAGMSG1=", None).await.unwrap().unwrap();
        assert_eq!(message.graph_id, "AAMkAGMSG1=");
        assert!(raw["body"]["content"].as_str().unwrap().contains("quarterly numbers"));
        assert!(client.get_message("gone", None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn get_summary_reads_a_row_without_a_body() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/m1"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(summary_json("m1")))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/gone"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;

        let client = client(&server);
        let summary = client.get_summary("m1", None).await.unwrap().unwrap();
        assert_eq!(summary.graph_id, "m1");
        assert_eq!(summary.subject.as_deref(), Some("Hello"));
        assert!(client.get_summary("gone", None).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn move_copy_and_delete_target_the_message() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/messages/m1/move"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .and(body_json(json!({ "destinationId": "deleteditems" })))
            .respond_with(ResponseTemplate::new(201).set_body_json(summary_json("m1")))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/messages/m1/copy"))
            .and(body_json(json!({ "destinationId": "archive-id" })))
            .respond_with(ResponseTemplate::new(201).set_body_json(summary_json("m1-copy")))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/messages/m1"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        assert_eq!(client.move_message("m1", "deleteditems").await.unwrap().unwrap().graph_id, "m1");
        assert_eq!(client.copy_message("m1", "archive-id").await.unwrap().unwrap().graph_id, "m1-copy");
        client.delete_message("m1").await.unwrap();
    }

    #[tokio::test]
    async fn update_message_sends_one_patch_and_skips_an_empty_one() {
        let server = MockServer::start().await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/messages/m1"))
            .and(body_json(json!({ "isRead": true, "importance": "high" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "m1" })))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let patch = MessagePatch {
            is_read: Some(true),
            importance: Some(Importance::High),
            ..Default::default()
        };
        client.update_message("m1", &patch, None).await.unwrap();
        client.update_message("m1", &MessagePatch::default(), None).await.unwrap();
    }

    #[tokio::test]
    async fn attachments_are_listed_and_read_by_id() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/m1/attachments"))
            .and(query_param("$select", "id,name,contentType,size,isInline"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::from_str::<Value>(include_str!("../../../../test-fixtures/outlook/message-attachments.json")).unwrap()),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages/m1/attachments/AAMkAGATT1%3D/$value"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"%PDF-1.7".to_vec()))
            .mount(&server)
            .await;

        let client = client(&server);
        let attachments = client.list_attachments("m1").await.unwrap();
        assert_eq!(attachments.len(), 2);
        assert_eq!(attachments[1].content_id.as_deref(), Some("chart1@contoso.com"));
        let bytes = client.get_attachment_bytes("m1", &attachments[0].id).await.unwrap();
        assert_eq!(bytes.as_deref(), Some(b"%PDF-1.7".as_slice()));
        assert!(client.get_attachment_bytes("m1", "missing").await.unwrap().is_none());
    }

    #[tokio::test]
    async fn drafts_are_created_updated_answered_and_sent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/messages"))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .and(body_partial_json(
                json!({ "subject": "Draft", "toRecipients": [{ "emailAddress": { "address": "a@b.example" } }] }),
            ))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "d1", "isDraft": true })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/messages/d1"))
            .and(body_partial_json(json!({ "subject": "Draft v2" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "d1" })))
            .expect(1)
            .mount(&server)
            .await;
        for (action, id) in [("createReply", "r1"), ("createReplyAll", "r2"), ("createForward", "f1")] {
            Mock::given(method("POST"))
                .and(path(format!("/v1.0/me/messages/m1/{action}")))
                .and(body_json(json!({ "comment": "See below" })))
                .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": id, "isDraft": true })))
                .mount(&server)
                .await;
        }
        Mock::given(method("POST"))
            .and(path("/v1.0/me/messages/d1/send"))
            .respond_with(ResponseTemplate::new(202))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        let draft = client.create_draft(&outgoing("Draft")).await.unwrap().unwrap();
        assert_eq!(draft.graph_id, "d1");
        assert!(draft.is_draft);
        client.update_draft("d1", &outgoing("Draft v2")).await.unwrap();
        assert_eq!(client.create_reply("m1", "See below").await.unwrap().unwrap().graph_id, "r1");
        assert_eq!(client.create_reply_all("m1", "See below").await.unwrap().unwrap().graph_id, "r2");
        assert_eq!(client.create_forward("m1", "See below").await.unwrap().unwrap().graph_id, "f1");
        client.send_draft("d1").await.unwrap();
        let requests = server.received_requests().await.unwrap();
        let send = requests.iter().find(|request| request.url.path() == "/v1.0/me/messages/d1/send").unwrap();
        assert!(send.body.is_empty(), "sending a draft posts no body");
    }

    #[tokio::test]
    async fn folders_are_created_renamed_and_deleted() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/mailFolders"))
            .and(body_json(json!({ "displayName": "Receipts" })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "f1", "displayName": "Receipts" })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1.0/me/mailFolders/f1/childFolders"))
            .and(body_json(json!({ "displayName": "2026" })))
            .respond_with(ResponseTemplate::new(201).set_body_json(json!({ "id": "f2", "displayName": "2026", "parentFolderId": "f1" })))
            .mount(&server)
            .await;
        Mock::given(method("PATCH"))
            .and(path("/v1.0/me/mailFolders/f2"))
            .and(body_json(json!({ "displayName": "Tax 2026" })))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "id": "f2" })))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path("/v1.0/me/mailFolders/f2"))
            .respond_with(ResponseTemplate::new(204))
            .expect(1)
            .mount(&server)
            .await;

        let client = client(&server);
        assert_eq!(client.create_folder(None, "Receipts").await.unwrap().unwrap().id, "f1");
        let child = client.create_folder(Some("f1"), "2026").await.unwrap().unwrap();
        assert_eq!(child.parent_id.as_deref(), Some("f1"));
        client.rename_folder("f2", "Tax 2026").await.unwrap();
        client.delete_folder("f2").await.unwrap();
    }

    #[tokio::test]
    async fn search_messages_quotes_the_text_and_caps_the_results() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/mailFolders/inbox/messages"))
            .and(query_param("$search", "\"q3 report\""))
            .and(header("Prefer", "IdType=\"ImmutableId\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [summary_json("m1"), summary_json("m2"), summary_json("m3")] })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1.0/me/messages"))
            .and(query_param("$search", "\"invoice\""))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "value": [summary_json("m9")] })))
            .mount(&server)
            .await;

        let client = client(&server);
        let found = client.search_messages(Some("inbox"), "q3 report", 2, None).await.unwrap();
        assert_eq!(found.iter().map(|m| m.graph_id.as_str()).collect::<Vec<_>>(), ["m1", "m2"]);
        let everywhere = client.search_messages(None, "invoice", 50, None).await.unwrap();
        assert_eq!(everywhere[0].graph_id, "m9");
    }

    #[test]
    fn encode_segment_escapes_path_specials() {
        assert_eq!(encode_segment("msg-1"), "msg-1");
        assert_eq!(encode_segment("a+b/c="), "a%2Bb%2Fc%3D");
    }
}
