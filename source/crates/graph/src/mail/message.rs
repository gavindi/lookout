//! Microsoft Graph ⇄ `lookout-core` message and folder mapping for the Graph
//! mail transport (MAIL_GRAPH_OVERVIEW.md item 5.7.2, A-06).
//!
//! Where the parent module maps the per-message Outlook *properties* layered on
//! IMAP (item 5.4), this one maps whole messages, folders and attachments, so a
//! Graph mail session (item 5.7.5) can stand in for the IMAP one. Like the
//! parent, it is pure - no HTTP - and every rule is unit-tested against inline
//! JSON and the `test-fixtures/outlook/` pages.
//!
//! The Graph records ([`GraphMessage`], [`MailFolder`], [`GraphAttachment`])
//! keep Graph's own ids. The mappers into `lookout_core` types take the
//! [`MailboxId`]/[`Uid`] to use as **parameters**: how a Graph id becomes a
//! `Uid` is the identity model's decision (item 5.7.3), not this module's.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use lookout_core::mailbox::guess_role_from_name;
use lookout_core::text::percent_encode;
use lookout_core::{
    auth_results_from_headers, follow_up_keywords, AccountId, BodyPart, EmailAddress, EmailBody, EmailSummary, FollowUpFlag, Importance, InferenceClassification, Mailbox,
    MailboxId, MailboxRole, MessageSensitivity, SystemFlagBit, ThreadKey, Uid, UidValidity,
};
use serde_json::{json, Map, Value};

use super::{categories_body, classification_body, flag_body, flag_from_graph, importance_body, non_empty_str, outgoing_message_body, string_array, OutgoingMessage};

/// The `$select` fields a message-list row needs - everything
/// [`message_from_graph`] reads, and no body.
pub const SUMMARY_SELECT: &str = "$select=id,changeKey,parentFolderId,conversationId,internetMessageId,subject,bodyPreview,from,toRecipients,ccRecipients,\
receivedDateTime,sentDateTime,isRead,isDraft,hasAttachments,importance,inferenceClassification,categories,flag";

/// The extra fields a message read adds for the reading pane
/// ([`email_body_from_graph`]): appended to [`SUMMARY_SELECT`]'s list.
pub const BODY_SELECT: &str = "body,internetMessageHeaders";

/// The `$select` fields of a folder read ([`mail_folder_from_graph`]).
pub const FOLDER_SELECT: &str = "$select=id,displayName,parentFolderId,childFolderCount,totalItemCount,unreadItemCount";

/// The `$select` fields of an attachment list ([`attachment_from_graph`]).
/// `contentBytes` is deliberately left out: the bytes are read on demand from
/// the attachment's `/$value`.
pub const ATTACHMENT_SELECT: &str = "$select=id,name,contentType,size,isInline";

/// Graph's well-known folder names, in [`WellKnownFolders`] field order. Each
/// resolves through `GET /me/mailFolders/{name}` to that mailbox's folder id
/// (Graph v1.0 doesn't report a folder's well-known name in a folder list).
pub const WELL_KNOWN_FOLDER_NAMES: [&str; 6] = ["inbox", "sentitems", "drafts", "deleteditems", "junkemail", "archive"];

/// One folder of a mailbox - the user's own (item 5.7.2) or a shared one
/// added by address (item 5.6 / M-42) - mapped to what the folder pane needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MailFolder {
    /// The folder's Graph id (`…/mailFolders/{folder_id}/messages`).
    pub id: String,
    pub display_name: String,
    /// The parent folder's Graph id, or `None` for a top-level folder. The
    /// mailbox root (`msgfolderroot`) is the parent of the top-level folders.
    pub parent_id: Option<String>,
    /// Total and unread item counts, for the folder row's badge.
    pub total: u32,
    pub unread: u32,
    /// How many folders sit directly under this one.
    pub child_count: u32,
}

/// Maps a Graph `mailFolder` to a [`MailFolder`], or `None` when it has no id
/// or name (a `/delta` tombstone included).
pub fn mail_folder_from_graph(value: &Value) -> Option<MailFolder> {
    if value.get("@removed").is_some() {
        return None;
    }
    let id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?;
    let display_name = value.get("displayName").and_then(Value::as_str).filter(|n| !n.trim().is_empty())?.to_string();
    let parent_id = value.get("parentFolderId").and_then(Value::as_str).filter(|p| !p.is_empty()).map(str::to_string);
    Some(MailFolder {
        id: id.to_string(),
        display_name,
        parent_id,
        total: count(value, "totalItemCount"),
        unread: count(value, "unreadItemCount"),
        child_count: count(value, "childFolderCount"),
    })
}

/// The folder ids behind Graph's well-known names (see
/// [`WELL_KNOWN_FOLDER_NAMES`]), `None` where the mailbox has no such folder
/// (an account with no Archive folder, say).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WellKnownFolders {
    pub inbox: Option<String>,
    pub sent: Option<String>,
    pub drafts: Option<String>,
    pub deleted: Option<String>,
    pub junk: Option<String>,
    pub archive: Option<String>,
}

impl WellKnownFolders {
    /// Records the id resolved for one of [`WELL_KNOWN_FOLDER_NAMES`]; an
    /// unknown name is ignored.
    pub fn set(&mut self, well_known_name: &str, folder_id: Option<String>) {
        let slot = match well_known_name {
            "inbox" => &mut self.inbox,
            "sentitems" => &mut self.sent,
            "drafts" => &mut self.drafts,
            "deleteditems" => &mut self.deleted,
            "junkemail" => &mut self.junk,
            "archive" => &mut self.archive,
            _ => return,
        };
        *slot = folder_id;
    }

    /// `folder`'s role: a well-known id match wins, since it holds whatever
    /// the folder is called in the mailbox's language; otherwise the same name
    /// guess the IMAP path falls back to.
    pub fn role_of(&self, folder: &MailFolder) -> MailboxRole {
        let is = |slot: &Option<String>| slot.as_deref() == Some(folder.id.as_str());
        if is(&self.inbox) {
            MailboxRole::Inbox
        } else if is(&self.sent) {
            MailboxRole::Sent
        } else if is(&self.drafts) {
            MailboxRole::Drafts
        } else if is(&self.deleted) {
            MailboxRole::Trash
        } else if is(&self.junk) {
            MailboxRole::Junk
        } else if is(&self.archive) {
            MailboxRole::Archive
        } else {
            guess_role_from_name(&folder.display_name)
        }
    }
}

/// Maps a [`MailFolder`] to the app's [`Mailbox`] under the ids the caller
/// assigns. Graph has no hierarchy delimiter, `UIDNEXT` or `MODSEQ`; `/` stands
/// in for the first, and the other two stay empty.
pub fn mailbox_from_graph(folder: &MailFolder, account_id: &AccountId, id: MailboxId, parent: Option<MailboxId>, role: MailboxRole, uidvalidity: UidValidity) -> Mailbox {
    Mailbox {
        id,
        account_id: account_id.clone(),
        name: folder.display_name.clone(),
        parent,
        delimiter: '/',
        role,
        uidvalidity,
        uidnext: 0,
        highest_modseq: None,
        total: folder.total,
        unread: folder.unread,
        flags: if folder.child_count > 0 { vec!["\\HasChildren".to_string()] } else { Vec::new() },
        subscribed: true,
    }
}

/// One message as a Graph message list or delta returns it
/// ([`SUMMARY_SELECT`]): the row-weight record, still keyed by Graph ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphMessage {
    /// The message's (immutable) Graph id.
    pub graph_id: String,
    /// Changes on every edit; lets a cache skip an unchanged upsert.
    pub change_key: Option<String>,
    /// The Graph id of the folder holding the message.
    pub folder_id: Option<String>,
    /// Graph's conversation id, the transport's thread key (Graph reports no
    /// `In-Reply-To`/`References` without reading the headers).
    pub conversation_id: Option<String>,
    /// The RFC 822 `Message-ID` (angle brackets included).
    pub message_id: Option<String>,
    pub subject: Option<String>,
    pub preview: Option<String>,
    pub from: Vec<EmailAddress>,
    pub to: Vec<EmailAddress>,
    pub cc: Vec<EmailAddress>,
    /// When the message arrived (`receivedDateTime`, else `sentDateTime`).
    pub received: Option<DateTime<Utc>>,
    pub is_read: bool,
    pub is_draft: bool,
    pub has_attachments: bool,
    /// Whether this is a meeting message (`#microsoft.graph.eventMessage`): an
    /// invitation, update, cancellation or response.
    pub is_meeting: bool,
    pub flag: Option<FollowUpFlag>,
    pub categories: Vec<String>,
    pub importance: Importance,
    pub classification: InferenceClassification,
}

/// Maps a Graph `message`, or `None` for a `/delta` tombstone or a record with
/// no id. `zone` interprets a flag's due date (see the parent module).
pub fn message_from_graph(value: &Value, zone: Option<chrono_tz::Tz>) -> Option<GraphMessage> {
    if value.get("@removed").is_some() {
        return None;
    }
    let graph_id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?.to_string();
    let received = ["receivedDateTime", "sentDateTime"]
        .iter()
        .find_map(|key| value.get(*key).and_then(Value::as_str).and_then(|raw| DateTime::parse_from_rfc3339(raw).ok()))
        .map(|at| at.with_timezone(&Utc));
    Some(GraphMessage {
        graph_id,
        change_key: non_empty_str(value, "changeKey"),
        folder_id: non_empty_str(value, "parentFolderId"),
        conversation_id: non_empty_str(value, "conversationId"),
        message_id: non_empty_str(value, "internetMessageId"),
        subject: non_empty_str(value, "subject"),
        preview: non_empty_str(value, "bodyPreview"),
        from: value.get("from").and_then(address_from_recipient).into_iter().collect(),
        to: addresses(value, "toRecipients"),
        cc: addresses(value, "ccRecipients"),
        received,
        is_read: flag_value(value, "isRead"),
        is_draft: flag_value(value, "isDraft"),
        has_attachments: flag_value(value, "hasAttachments"),
        is_meeting: value.get("@odata.type").and_then(Value::as_str) == Some("#microsoft.graph.eventMessage"),
        flag: value.get("flag").and_then(|flag| flag_from_graph(flag, zone)),
        categories: string_array(value, "categories"),
        importance: value.get("importance").and_then(Value::as_str).and_then(Importance::from_graph).unwrap_or_default(),
        classification: value
            .get("inferenceClassification")
            .and_then(Value::as_str)
            .and_then(InferenceClassification::from_graph)
            .unwrap_or_default(),
    })
}

/// Maps a [`GraphMessage`] to the app's [`EmailSummary`] under the ids the
/// caller assigns.
///
/// - `isRead` and `isDraft` become `\Seen` and `\Draft`. The follow-up flag
///   becomes the `$Lookout-flag-*` keywords the IMAP path reads; Pin
///   (`\Flagged`) has no Graph counterpart and stays off.
/// - The thread key is `graph:{conversationId}`, since a Graph summary carries
///   no `In-Reply-To`/`References` for `compute_thread_keys` to link.
/// - `size` is 0 and `structure` is `None`: Graph v1.0 reports neither without
///   extended properties or a MIME read.
/// - Categories aren't on `EmailSummary`; they stay on the [`GraphMessage`].
pub fn email_summary_from_graph(message: &GraphMessage, mailbox: MailboxId, uid: Uid) -> EmailSummary {
    let mut flags = BTreeSet::new();
    if message.is_read {
        flags.insert(SystemFlagBit::Seen);
    }
    if message.is_draft {
        flags.insert(SystemFlagBit::Draft);
    }
    let keywords = message.flag.as_ref().map(follow_up_keywords).unwrap_or_default().into_iter().collect();
    EmailSummary {
        uid,
        mailbox,
        message_id: message.message_id.clone(),
        in_reply_to: None,
        references: Vec::new(),
        thread_key: ThreadKey(message.conversation_id.as_deref().map(|id| format!("graph:{id}")).unwrap_or_default()),
        subject: message.subject.clone(),
        from: message.from.clone(),
        to: message.to.clone(),
        cc: message.cc.clone(),
        date: message.received.unwrap_or(DateTime::UNIX_EPOCH),
        flags,
        keywords,
        size: 0,
        has_attachment: message.has_attachments,
        has_calendar: message.is_meeting,
        preview: message.preview.clone(),
        mentions: false,
        importance: message.importance,
        sensitivity: MessageSensitivity::default(),
        inference_classification: message.classification,
        list_id: None,
        structure: None,
    }
}

/// One read of a folder's messages delta at summary weight
/// ([`SUMMARY_SELECT`]): live records, the Graph ids of tombstones (deleted or
/// moved out), and the cursor for the next read.
#[derive(Debug, Clone, Default)]
pub struct MessageSummaryDelta {
    pub upserts: Vec<GraphMessage>,
    pub removals: Vec<String>,
    pub delta_link: Option<String>,
}

/// One read of the mailbox's folder delta: every folder in the hierarchy on
/// the first read, then only the changes.
#[derive(Debug, Clone, Default)]
pub struct MailFolderDelta {
    pub folders: Vec<MailFolder>,
    pub removals: Vec<String>,
    pub delta_link: Option<String>,
}

/// One attachment as a Graph attachment list returns it ([`ATTACHMENT_SELECT`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GraphAttachment {
    /// The attachment's Graph id (`…/attachments/{id}/$value`).
    pub id: String,
    pub name: Option<String>,
    pub content_type: String,
    pub size: u32,
    pub is_inline: bool,
    /// The `Content-ID` an inline image's `cid:` reference resolves to, when
    /// Graph reports one.
    pub content_id: Option<String>,
}

/// Maps a Graph `attachment`, or `None` when it has no id.
pub fn attachment_from_graph(value: &Value) -> Option<GraphAttachment> {
    let id = value.get("id").and_then(Value::as_str).filter(|id| !id.is_empty())?.to_string();
    Some(GraphAttachment {
        id,
        name: non_empty_str(value, "name"),
        content_type: non_empty_str(value, "contentType").map_or_else(|| "application/octet-stream".to_string(), |c| c.to_ascii_lowercase()),
        size: count(value, "size"),
        is_inline: flag_value(value, "isInline"),
        content_id: non_empty_str(value, "contentId"),
    })
}

/// A [`GraphAttachment`] as a [`BodyPart`]. On the Graph transport a part's
/// `part_number` is the Graph attachment id in filename-safe form
/// ([`super::graph_part_number`]), not an IMAP body-part path;
/// [`super::attachment_id_from_part`] recovers the id the
/// `/attachments/{id}/$value` read takes. Graph hands out decoded bytes, so
/// there is no charset or transfer encoding.
pub fn body_part_from_attachment(attachment: &GraphAttachment) -> BodyPart {
    BodyPart {
        part_number: super::graph_part_number(&attachment.id),
        content_type: attachment.content_type.clone(),
        charset: None,
        transfer_encoding: None,
        filename: attachment.name.clone(),
        cid: attachment.content_id.clone(),
        size: attachment.size,
        is_attachment: !attachment.is_inline,
    }
}

/// The reading-pane body of a Graph message read with [`BODY_SELECT`], plus
/// its attachments' metadata. An HTML body fills `html_body` and anything else
/// `text_body`; the headers come from `internetMessageHeaders`. The iMIP
/// `text/calendar` payload isn't in Graph's JSON body, so `calendar_ics` stays
/// `None`; that needs the MIME read (`MailClient::get_mime`).
pub fn email_body_from_graph(value: &Value, uid: Uid, attachments: &[GraphAttachment]) -> EmailBody {
    let body = value.get("body");
    let content = body.and_then(|body| body.get("content")).and_then(Value::as_str).map(str::to_string);
    let is_html = body
        .and_then(|body| body.get("contentType"))
        .and_then(Value::as_str)
        .is_some_and(|kind| kind.eq_ignore_ascii_case("html"));
    let headers: Vec<(String, String)> = value
        .get("internetMessageHeaders")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|header| Some((header.get("name")?.as_str()?.to_string(), header.get("value")?.as_str()?.to_string())))
                .collect()
        })
        .unwrap_or_default();
    EmailBody {
        uid,
        text_body: if is_html { None } else { content.clone() },
        html_body: if is_html { content } else { None },
        calendar_ics: None,
        parts: attachments.iter().map(body_part_from_attachment).collect(),
        auth_results: auth_results_from_headers(&headers),
        headers,
    }
}

/// A partial message update: only the `Some` fields are written, all in one
/// `PATCH`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MessagePatch {
    pub is_read: Option<bool>,
    pub flag: Option<FollowUpFlag>,
    pub categories: Option<Vec<String>>,
    pub importance: Option<Importance>,
    pub classification: Option<InferenceClassification>,
}

impl MessagePatch {
    /// Whether the patch would write nothing.
    pub fn is_empty(&self) -> bool {
        *self == MessagePatch::default()
    }

    /// The `PATCH` body. `zone` places a flag's due date (see the parent
    /// module).
    pub fn body(&self, zone: Option<chrono_tz::Tz>) -> Value {
        let mut body = Map::new();
        if let Some(is_read) = self.is_read {
            body.insert("isRead".to_string(), json!(is_read));
        }
        if let Some(flag) = &self.flag {
            body.insert("flag".to_string(), flag_body(flag, zone));
        }
        let parts = [
            self.categories.as_deref().map(categories_body),
            self.importance.map(importance_body),
            self.classification.map(classification_body),
        ];
        for part in parts.into_iter().flatten() {
            if let Value::Object(fields) = part {
                body.extend(fields);
            }
        }
        Value::Object(body)
    }
}

/// The body of a message `move` or `copy` to `folder_id` (a folder id or a
/// well-known name).
pub fn destination_body(folder_id: &str) -> Value {
    json!({ "destinationId": folder_id })
}

/// The body that creates or renames a folder.
pub fn folder_name_body(name: &str) -> Value {
    json!({ "displayName": name })
}

/// The body that creates (or rewrites) a draft: the same `message` JSON
/// `sendMail` takes, without a deferred send time. Graph caps a request at
/// about 4 MB, so larger attachments would need an upload session, which this
/// transport doesn't do.
pub fn draft_body(message: &OutgoingMessage) -> Value {
    outgoing_message_body(message, None)
}

/// The body of a `createReply`/`createReplyAll`/`createForward`: the text
/// Graph puts above the quoted original.
pub fn comment_body(comment: &str) -> Value {
    json!({ "comment": comment })
}

/// The `$search` query option for a free-text message search, quoted and
/// percent-encoded. Graph's message search is KQL inside double quotes, so a
/// `"` or `\` in the text is backslash-escaped.
pub fn search_param(text: &str) -> String {
    let escaped = text.trim().replace('\\', "\\\\").replace('"', "\\\"");
    format!("$search={}", percent_encode(&format!("\"{escaped}\"")))
}

fn count(value: &Value, key: &str) -> u32 {
    value.get(key).and_then(Value::as_u64).unwrap_or(0).min(u32::MAX as u64) as u32
}

fn flag_value(value: &Value, key: &str) -> bool {
    value.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// A Graph `recipient` (`{"emailAddress": {"name", "address"}}`), or `None`
/// without an address.
fn address_from_recipient(value: &Value) -> Option<EmailAddress> {
    let email = value.get("emailAddress")?;
    let address = email
        .get("address")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|address| !address.is_empty())?
        .to_string();
    Some(EmailAddress {
        name: non_empty_str(email, "name"),
        address,
    })
}

fn addresses(value: &Value, key: &str) -> Vec<EmailAddress> {
    value
        .get(key)
        .and_then(Value::as_array)
        .map(|recipients| recipients.iter().filter_map(address_from_recipient).collect())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use lookout_core::{FlagStatus, FLAG_KEYWORD};

    fn fixture(name: &str) -> Value {
        let text = match name {
            "folders" => include_str!("../../../../test-fixtures/outlook/mail-folders-delta.json"),
            "delta" => include_str!("../../../../test-fixtures/outlook/message-summary-delta.json"),
            "full" => include_str!("../../../../test-fixtures/outlook/message-full.json"),
            "attachments" => include_str!("../../../../test-fixtures/outlook/message-attachments.json"),
            _ => unreachable!(),
        };
        serde_json::from_str(text).unwrap()
    }

    fn account() -> AccountId {
        AccountId("m365".to_string())
    }

    #[test]
    fn the_folders_delta_fixture_maps_and_skips_the_tombstone() {
        let page = fixture("folders");
        let folders: Vec<MailFolder> = page["value"].as_array().unwrap().iter().filter_map(mail_folder_from_graph).collect();
        assert_eq!(folders.len(), 4);
        assert_eq!(folders[0].display_name, "Inbox");
        assert_eq!(folders[0].child_count, 1);
        assert_eq!(folders[1].parent_id.as_deref(), Some("AAMkAGINBOX="));
        assert_eq!(folders[1].unread, 2);
    }

    #[test]
    fn a_well_known_id_beats_the_name_guess() {
        let mut well_known = WellKnownFolders::default();
        well_known.set("inbox", Some("AAMkAGINBOX=".to_string()));
        well_known.set("deleteditems", Some("f-trash".to_string()));
        well_known.set("nonsense", Some("ignored".to_string()));
        let folder = |id: &str, name: &str| MailFolder {
            id: id.to_string(),
            display_name: name.to_string(),
            parent_id: None,
            total: 0,
            unread: 0,
            child_count: 0,
        };
        // A localised name still maps through the well-known id.
        assert_eq!(well_known.role_of(&folder("f-trash", "Gelöschte Elemente")), MailboxRole::Trash);
        assert_eq!(well_known.role_of(&folder("AAMkAGINBOX=", "Posteingang")), MailboxRole::Inbox);
        // No id match falls back to the IMAP path's name guess.
        assert_eq!(well_known.role_of(&folder("f-sent", "Sent Items")), MailboxRole::Sent);
        assert_eq!(well_known.role_of(&folder("f-x", "Projects")), MailboxRole::Custom);
    }

    #[test]
    fn a_folder_maps_to_a_mailbox_under_the_given_ids() {
        let page = fixture("folders");
        let inbox = mail_folder_from_graph(&page["value"][0]).unwrap();
        let projects = mail_folder_from_graph(&page["value"][1]).unwrap();
        let inbox_id = MailboxId::new(&account(), "AAMkAGINBOX=");

        let mailbox = mailbox_from_graph(&inbox, &account(), inbox_id.clone(), None, MailboxRole::Inbox, UidValidity(3));
        assert_eq!(mailbox.id, inbox_id);
        assert_eq!(mailbox.name, "Inbox");
        assert_eq!(mailbox.total, 120);
        assert_eq!(mailbox.unread, 7);
        assert_eq!(mailbox.uidvalidity, UidValidity(3));
        assert_eq!(mailbox.flags, vec!["\\HasChildren".to_string()]);

        let child = mailbox_from_graph(
            &projects,
            &account(),
            MailboxId::new(&account(), "AAMkAGPROJECTS="),
            Some(inbox_id.clone()),
            MailboxRole::Custom,
            UidValidity(3),
        );
        assert_eq!(child.parent, Some(inbox_id));
        assert!(child.flags.is_empty());
    }

    #[test]
    fn the_summary_delta_fixture_maps_every_record_shape() {
        let page = fixture("delta");
        let items = page["value"].as_array().unwrap();
        let messages: Vec<GraphMessage> = items.iter().filter_map(|item| message_from_graph(item, None)).collect();
        assert_eq!(messages.len(), 4, "the tombstone is skipped");
        assert_eq!(super::super::removed_id(&items[4]).as_deref(), Some("AAMkAGGONE="));

        let report = &messages[0];
        assert_eq!(report.graph_id, "AAMkAGMSG1=");
        assert_eq!(report.folder_id.as_deref(), Some("AAMkAGINBOX="));
        assert_eq!(report.from[0].name.as_deref(), Some("Adele Vance"));
        assert_eq!(report.to.len(), 2);
        assert_eq!(report.to[1].name, None);
        assert!(!report.is_read);
        assert!(report.has_attachments);
        assert_eq!(report.importance, Importance::High);
        assert_eq!(report.categories, vec!["Work".to_string()]);
        assert_eq!(report.flag.as_ref().unwrap().status, FlagStatus::Flagged);
        assert_eq!(report.received, Some("2026-10-14T09:30:00Z".parse().unwrap()));

        assert!(messages[1].is_read);
        assert_eq!(messages[1].flag, None, "notFlagged reads as no flag");
        assert_eq!(messages[1].classification, InferenceClassification::Other);
        assert!(messages[2].is_draft);
        assert!(messages[2].from.is_empty());
        let invite = &messages[3];
        assert!(invite.is_meeting);
        assert_eq!(invite.received, Some("2026-10-15T11:00:00Z".parse().unwrap()), "falls back to sentDateTime");
    }

    #[test]
    fn a_message_maps_to_a_summary_under_the_given_ids() {
        let page = fixture("delta");
        let report = message_from_graph(&page["value"][0], None).unwrap();
        let mailbox = MailboxId::new(&account(), "AAMkAGINBOX=");

        let summary = email_summary_from_graph(&report, mailbox.clone(), Uid(42));
        assert_eq!(summary.uid, Uid(42));
        assert_eq!(summary.mailbox, mailbox);
        assert_eq!(summary.message_id.as_deref(), Some("<report-1@contoso.com>"));
        assert_eq!(summary.thread_key, ThreadKey("graph:AAQkAGCONV1=".to_string()));
        assert!(summary.is_unread());
        assert!(!summary.is_pinned(), "a Graph flag is a follow-up flag, not Pin");
        assert!(summary.keywords.contains(FLAG_KEYWORD));
        assert!(summary.is_flagged_for_follow_up());
        assert_eq!(summary.follow_up(), report.flag.clone());
        assert!(summary.has_attachment);
        assert_eq!(summary.importance, Importance::High);
        assert_eq!(summary.preview.as_deref(), Some("Hi team, the quarterly numbers are in."));
        assert_eq!(summary.size, 0);
        assert!(summary.structure.is_none());

        let draft = email_summary_from_graph(&message_from_graph(&page["value"][2], None).unwrap(), mailbox.clone(), Uid(1));
        assert!(draft.flags.contains(&SystemFlagBit::Draft));
        assert!(draft.flags.contains(&SystemFlagBit::Seen));
        assert!(draft.keywords.is_empty());
        let invite = email_summary_from_graph(&message_from_graph(&page["value"][3], None).unwrap(), mailbox, Uid(2));
        assert!(invite.has_calendar);
    }

    #[test]
    fn tombstones_and_id_less_messages_are_skipped() {
        assert!(message_from_graph(&json!({ "id": "gone", "@removed": { "reason": "deleted" } }), None).is_none());
        assert!(message_from_graph(&json!({ "subject": "no id" }), None).is_none());
        let bare = message_from_graph(&json!({ "id": "m1" }), None).unwrap();
        assert_eq!(bare.received, None);
        assert!(!bare.is_read);
        assert_eq!(bare.importance, Importance::Normal);
        assert_eq!(
            email_summary_from_graph(&bare, MailboxId::new(&account(), "f"), Uid(1)).thread_key,
            ThreadKey(String::new())
        );
    }

    #[test]
    fn attachments_become_parts_keyed_by_their_graph_id() {
        let page = fixture("attachments");
        let attachments: Vec<GraphAttachment> = page["value"].as_array().unwrap().iter().filter_map(attachment_from_graph).collect();
        assert_eq!(attachments.len(), 2);

        let pdf = body_part_from_attachment(&attachments[0]);
        assert_eq!(pdf.part_number, "AAMkAGATT1=");
        let slashed = body_part_from_attachment(&attachment_from_graph(&json!({ "id": "AA+k/ATT3=" })).unwrap());
        assert_eq!(slashed.part_number, "AA-k_ATT3=");
        assert_eq!(super::super::attachment_id_from_part(&slashed.part_number).as_deref(), Some("AA+k/ATT3="));
        assert_eq!(pdf.content_type, "application/pdf");
        assert_eq!(pdf.filename.as_deref(), Some("Q3-report.pdf"));
        assert_eq!(pdf.size, 48213);
        assert!(pdf.is_attachment);

        let chart = body_part_from_attachment(&attachments[1]);
        assert!(!chart.is_attachment);
        assert_eq!(chart.cid.as_deref(), Some("chart1@contoso.com"));
        assert!(attachment_from_graph(&json!({ "name": "no id" })).is_none());
        assert_eq!(attachment_from_graph(&json!({ "id": "a" })).unwrap().content_type, "application/octet-stream");
    }

    #[test]
    fn the_full_message_fixture_maps_to_a_body() {
        let page = fixture("attachments");
        let attachments: Vec<GraphAttachment> = page["value"].as_array().unwrap().iter().filter_map(attachment_from_graph).collect();
        let body = email_body_from_graph(&fixture("full"), Uid(42), &attachments);
        assert_eq!(body.uid, Uid(42));
        assert!(body.html_body.as_deref().unwrap().contains("cid:chart1@contoso.com"));
        assert_eq!(body.text_body, None);
        assert_eq!(body.calendar_ics, None);
        assert_eq!(body.parts.len(), 2);
        assert_eq!(lookout_core::header_value(&body.headers, "list-id"), Some("Finance <finance.contoso.com>"));
        let auth = body.auth_results.unwrap();
        assert!(!auth.any_failure());
        assert!(!auth.is_empty());

        let text = email_body_from_graph(&json!({ "body": { "contentType": "text", "content": "plain" } }), Uid(1), &[]);
        assert_eq!(text.text_body.as_deref(), Some("plain"));
        assert_eq!(text.html_body, None);
        assert!(text.headers.is_empty());
        assert_eq!(text.auth_results, None);
    }

    #[test]
    fn a_message_patch_writes_only_its_fields_in_one_body() {
        assert!(MessagePatch::default().is_empty());
        assert_eq!(MessagePatch::default().body(None), json!({}));

        let patch = MessagePatch {
            is_read: Some(true),
            categories: Some(vec!["Work".to_string()]),
            importance: Some(Importance::Low),
            classification: Some(InferenceClassification::Other),
            flag: Some(FollowUpFlag::default()),
        };
        assert!(!patch.is_empty());
        assert_eq!(
            patch.body(None),
            json!({
                "isRead": true,
                "categories": ["Work"],
                "importance": "low",
                "inferenceClassification": "other",
                "flag": { "flagStatus": "notFlagged" }
            })
        );
        let read_only = MessagePatch {
            is_read: Some(false),
            ..Default::default()
        };
        assert_eq!(read_only.body(None), json!({ "isRead": false }));
    }

    #[test]
    fn the_small_request_bodies() {
        assert_eq!(destination_body("deleteditems"), json!({ "destinationId": "deleteditems" }));
        assert_eq!(folder_name_body("Receipts"), json!({ "displayName": "Receipts" }));
        assert_eq!(comment_body("See below"), json!({ "comment": "See below" }));
    }

    #[test]
    fn search_param_quotes_escapes_and_encodes() {
        assert_eq!(search_param("report"), "$search=%22report%22");
        assert_eq!(search_param("  q3 report "), "$search=%22q3%20report%22");
        assert_eq!(search_param("say \"hi\""), "$search=%22say%20%5C%22hi%5C%22%22");
        assert_eq!(search_param("a\\b"), "$search=%22a%5C%5Cb%22");
    }
}
