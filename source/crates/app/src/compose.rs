/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use base64::Engine;
use gtk::{gio, glib};
use lookout_core::{header_value, AccountId, EmailAddress, EmailBody, EmailSummary, Identity, Importance, MailTemplate, MailboxId, MessageSensitivity, Signature, Uid};
use lookout_mail::body::DraftPrefillContent;
use lookout_mail::session::{AccountCommand, DraftReplace};
use lookout_mail::{new_message_id, Attachment, ComposedMessage, InlineImage};
use webkit::prelude::*;

use crate::recipient_entry::{RecipientEntry, Suggestion, SuggestionSource};
use crate::util::escape_html;

/// How long the composer waits after the last check before autosaving the
/// draft again. The autosave runs on a fixed tick and compares the current
/// fields against the last saved snapshot, so this is the worst-case gap
/// between an edit and its draft landing server-side. A draft save is
/// several IMAP round trips (replace + append), so this errs slow.
const DRAFT_AUTOSAVE_INTERVAL: Duration = Duration::from_secs(5);

/// Everything the window layer needs to move a composer between the reading
/// pane and its pop-out window. `widget` is the composer root the window
/// layer reparents; the flags are the composer's own session state, which
/// must follow it across the move:
///
/// * `moving` is set by whoever performs the reparent (the composer itself
///   for pop-out, the window layer for pop-back-in) around the removal, so
///   the composer's `root-notify` handler can tell an intentional move apart
///   from being displaced by a second composer (which must stop the
///   autosave).
/// * `popped_out` is true while the composer lives in the pop-out window;
///   the composer hides its pop-out button while set, and the window layer
///   clears it before popping back in.
/// * `done` is the composer's `closed` flag - the window layer's X button
///   pops the composer back in only while it's still alive, and just closes
///   the window once Send or Cancel has already finished the session.
///
/// The window layer also relocates the header row's Cancel, Send, From and
/// read-receipt-toggle widgets into the pop-out window's title bar (`top_row`
/// is where they normally live, so they can be returned on pop-back-in); the
/// widgets keep their click/state handlers across the move, since they're
/// the same widgets.
///
/// While the composer is embedded in the reading pane the editor toolbar
/// lives in the main window's action bar (see `ComposeActionBar` in
/// `window.rs`); `editor_toolbar` is the toolbar itself and `toolbar_anchor`
/// the widget it normally follows in the composer column (the attachment
/// flow), so the window layer can return the toolbar to the composer on
/// pop-out and lift it back out on pop-back-in.
pub struct PopOutHandle {
    pub widget: gtk::Box,
    /// The compose editor's toolbar row (attach, plain/rich toggle, and the
    /// rich-text formatting buttons). Relocated into the composer by the
    /// pop-out handler and back into the action bar by the pop-back-in
    /// handler (see `ComposeActionBar` in `window.rs`).
    pub editor_toolbar: gtk::Box,
    /// The composer-column widget the editor toolbar sits directly after -
    /// the anchor for re-inserting the toolbar into the composer on pop-out.
    pub toolbar_anchor: gtk::Widget,
    pub cancel_button: gtk::Button,
    pub send_button: adw::SplitButton,
    pub from_dropdown: gtk::DropDown,
    pub read_receipt_toggle: gtk::ToggleButton,
    /// The composer's own header row, which the widgets above are removed
    /// from while popped out and restored to on pop-back-in.
    pub top_row: gtk::Box,
    /// The header row's title and draft-status labels. While popped out the
    /// whole row is hidden (the window's title bar duplicates the title), so
    /// the status label is relocated into the title bar; both are restored
    /// to the header row on pop-back-in.
    pub title_label: gtk::Label,
    pub status_label: gtk::Label,
    pub moving: Rc<Cell<bool>>,
    pub popped_out: Rc<Cell<bool>>,
    pub done: Rc<Cell<bool>>,
}

/// Everything the compose window can be pre-filled with, beyond a blank "New
/// Message" (`ComposePrefill::default()`). Grouped into one struct rather
/// than more loose parameters since Reply/Reply-All/Forward all need to set
/// several of these fields together. Reopening a saved draft (item 1.12)
/// uses the same channel for everything a draft restores: the HTML body,
/// Bcc/attachment/importance/read-receipt fields Reply/Forward can't know,
/// the identity it was written as, and the draft identity the autosave
/// replaces on save.
#[derive(Default)]
pub struct ComposePrefill {
    pub to: Option<String>,
    pub cc: Option<String>,
    /// Blind-copy recipients. Only ever set when reopening a draft - a draft
    /// stores its Bcc as a header (see `build_draft_raw_message`); Reply and
    /// Forward, by definition, cannot know a blind-copy list.
    pub bcc: Option<String>,
    pub subject: Option<String>,
    /// The body as plain text. For a draft this is the saved text part; for
    /// Reply/Forward it is the quoted/forwarded plain text.
    pub body: Option<String>,
    /// The exact HTML body for the rich editor. Set when reopening a draft
    /// whose saved form was HTML (with `cid:` images already rewritten to
    /// `data:` URIs - see `lookout_mail::body::parse_draft_prefill`), and by
    /// Forward when the original carried an HTML body (item 1.11 / C-13; the
    /// forward launcher rewrites its `cid:` references too). `None` for every
    /// other prefill, where the rich editor derives its markup from `body` via
    /// `text_to_html`.
    pub html_body: Option<String>,
    /// The sending identity's address to preselect in the From dropdown,
    /// matched case-insensitively against the account's identities - a draft
    /// reopened after being written as an alternate identity must not
    /// silently switch to the default one. An address that no longer matches
    /// any identity leaves the default selected.
    pub from: Option<String>,
    /// The saved importance. `Normal` (the default) for non-draft prefills.
    pub importance: Importance,
    /// The saved RFC 2156 `Sensitivity`. `Normal` (the default) for
    /// non-draft prefills and for drafts that carried no `Sensitivity` header.
    pub sensitivity: MessageSensitivity,
    /// Whether the saved draft asked for a read receipt
    /// (`Disposition-Notification-To`).
    pub request_read_receipt: bool,
    /// Files to seed the composer's attachment list with, bytes included
    /// (decoded from the saved draft's MIME parts by
    /// `parse_draft_prefill`).
    pub attachments: Vec<Attachment>,
    /// A reopened draft's bare `Message-ID` (no `<>`), which becomes the
    /// autosave's stable id so every resave replaces the stored copy instead
    /// of appending a second one. `None` for a fresh composer (a new id is
    /// generated) and for foreign drafts whose saved form carried no
    /// `Message-ID`.
    pub draft_message_id: Option<String>,
    /// A reopened draft's source `(mailbox, uid)`: the first autosave purges
    /// exactly this copy before appending the new one, so a foreign draft
    /// without a `Message-ID` still replaces rather than duplicates.
    /// Consumed by that first save; later saves purge by `Message-ID`.
    pub draft_source: Option<(MailboxId, Uid)>,
    /// The draft is already stored in Drafts under `draft_message_id` - a
    /// message whose send was undone (item 1.13), whose autosaved draft the
    /// outbox hadn't deleted yet. Its first autosave then replaces that copy
    /// by `Message-ID` instead of appending a second one.
    pub draft_saved: bool,
    /// RFC 5322 `In-Reply-To` - the bare Message-Id (no `<>`) of the message
    /// being replied to. `mail_builder`'s `MessageId` header writer adds the
    /// angle brackets itself, so this must NOT include them.
    pub in_reply_to: Option<String>,
    /// RFC 5322 `References` - bare Message-Ids (no `<>`), oldest first.
    pub references: Vec<String>,
    /// The iMIP part sent alongside the body (CAL-15): a full
    /// `text/calendar` document, set when the attendee edits their
    /// invitation response in the composer. `None` for every other message.
    /// `mail/src/send.rs` emits it as the second part of a
    /// `multipart/alternative` [text/plain, text/calendar] and ignores the
    /// HTML body, so this flow opens the composer in plain-text mode.
    pub calendar_part: Option<String>,
}

/// Runs as Send is clicked (or a schedule-send time is picked, item 1.14),
/// with the message about to be queued and its scheduled time, if any: shows
/// the "Sending" feedback (with Undo during the undo-send delay, item 1.13)
/// or the "Scheduled for …" one, and returns the `SendMessage`'s
/// `not_before` and `client_token`.
pub type SendStarted = Rc<dyn Fn(&ComposedMessage, Option<chrono::DateTime<chrono::Utc>>) -> (Option<chrono::DateTime<chrono::Utc>>, Option<u64>)>;

/// The server-side schedule-send hook (item 5.5 / C-08): given the message and
/// its scheduled time, returns true when it was scheduled through Exchange (so
/// nothing goes in the local outbox), false to fall back to it.
pub type ScheduleSendHook = Rc<dyn Fn(&ComposedMessage, chrono::DateTime<chrono::Utc>) -> bool>;

/// Whether Reply excludes the original's other recipients (`Reply`) or
/// carries them all forward minus the replying account itself (`ReplyAll`).
#[derive(Clone, Copy)]
pub enum ReplyMode {
    Reply,
    ReplyAll,
}

/// Strips one layer of RFC 5322 `<...>` angle brackets from a Message-Id
/// token, if present. `mail_builder`'s `MessageId` header writer adds its
/// own brackets around each id it's given, so a raw header value (which
/// already has them) must be stripped first or the sent header would come
/// out double-bracketed.
pub(crate) fn strip_angle_brackets(id: &str) -> &str {
    id.trim().trim_start_matches('<').trim_end_matches('>')
}

fn own_address_matches(address: &str, own_email: &str) -> bool {
    address.eq_ignore_ascii_case(own_email)
}

fn with_prefix_once(subject: &str, prefix: &str) -> String {
    let trimmed = subject.trim();
    if trimmed.len() >= prefix.len() && trimmed[..prefix.len()].eq_ignore_ascii_case(prefix) {
        trimmed.to_string()
    } else {
        format!("{prefix}{trimmed}")
    }
}

fn quote_lines(text: &str) -> String {
    text.lines().map(|line| format!("> {line}")).collect::<Vec<_>>().join("\n")
}

fn sender_label(summary: &EmailSummary) -> String {
    summary.from.first().map(|a| a.display_label().to_string()).unwrap_or_else(|| "someone".to_string())
}

/// Builds the `in_reply_to`/`references` chain for a reply from the
/// original message's raw headers (never from `EmailSummary::references`,
/// which is always empty - see `header_value`'s doc comment).
fn reply_threading(body: &EmailBody) -> (Option<String>, Vec<String>) {
    let original_id = header_value(&body.headers, "message-id").map(strip_angle_brackets);
    let mut references: Vec<String> = header_value(&body.headers, "references")
        .map(|v| v.split_whitespace().map(strip_angle_brackets).map(str::to_string).collect())
        .unwrap_or_default();
    if let Some(id) = original_id {
        references.push(id.to_string());
    }
    (original_id.map(str::to_string), references)
}

/// Builds the Reply/Reply-All prefill: recipients (bare addresses, own
/// address excluded), a `Re: `-prefixed subject, the quoted original body,
/// and the Message-Id/References chain so the reply threads correctly.
pub fn build_reply_prefill(summary: &EmailSummary, body: &EmailBody, own_email: &str, mode: ReplyMode) -> ComposePrefill {
    let mut to: Vec<String> = summary.from.iter().map(|a| a.address.clone()).collect();
    let mut cc: Vec<String> = Vec::new();
    if matches!(mode, ReplyMode::ReplyAll) {
        to.extend(summary.to.iter().map(|a| a.address.clone()));
        cc.extend(summary.cc.iter().map(|a| a.address.clone()));
    }
    to.retain(|a| !own_address_matches(a, own_email));
    cc.retain(|a| !own_address_matches(a, own_email));

    let subject = summary.subject.as_deref().map(|s| with_prefix_once(s, "Re: ")).unwrap_or_else(|| "Re: ".to_string());

    let quoted = body.text_body.as_deref().unwrap_or("");
    let reply_body = format!(
        "\n\nOn {}, {} wrote:\n{}",
        summary.date.format("%a, %b %d, %Y at %I:%M %p"),
        sender_label(summary),
        quote_lines(quoted)
    );

    let (in_reply_to, references) = reply_threading(body);

    ComposePrefill {
        to: Some(to.join(", ")),
        cc: if cc.is_empty() { None } else { Some(cc.join(", ")) },
        subject: Some(subject),
        body: Some(reply_body),
        in_reply_to,
        references,
        ..Default::default()
    }
}

/// Builds the Forward prefill: blank recipients (the user fills these in), a
/// `Fwd: `-prefixed subject, and a forwarded-message header block followed
/// by the original body. Unlike Reply, this starts a new, unthreaded
/// conversation - no `in_reply_to`/`references` are set.
///
/// Two bodies are produced. `body` is the plain-text form (the send-time
/// fallback and the source for the editor's plain mode). When the original
/// carried an HTML body, `html_body` is set too - the same header block
/// followed by the original HTML in a `<blockquote>`, so a rich original
/// forwards with its formatting intact. Its inline `cid:` image references
/// are left as-is here; the forward launcher (`window.rs`) fetches the
/// referenced parts and rewrites them to `data:` URIs (see
/// `lookout_mail::body::rewrite_cid_refs_with`) before the composer opens, so
/// the images survive. Attachments are likewise filled in by the launcher
/// after it fetches the original's parts (item 1.11 / C-13).
///
/// The forwarded header block names the original's own `From:`/`To:` even
/// though a `Fwd:` message goes to new recipients.
pub fn build_forward_prefill(summary: &EmailSummary, body: &EmailBody) -> ComposePrefill {
    let subject = summary.subject.as_deref().map(|s| with_prefix_once(s, "Fwd: ")).unwrap_or_else(|| "Fwd: ".to_string());

    let from = summary.from.iter().map(|a| a.display_label()).collect::<Vec<_>>().join(", ");
    let to = summary.to.iter().map(|a| a.display_label()).collect::<Vec<_>>().join(", ");
    let date = summary.date.format("%a, %b %d, %Y at %I:%M %p").to_string();
    let original_subject = summary.subject.as_deref().unwrap_or("");
    let original = body.text_body.as_deref().unwrap_or("");
    let forward_body = format!("\n\n---------- Forwarded message ----------\nFrom: {from}\nDate: {date}\nSubject: {original_subject}\nTo: {to}\n\n{original}");

    // The HTML companion forwards the original's HTML, not the plain text, so
    // its formatting is preserved. The header block is the HTML rendering of
    // the same fields.
    let html_body = body.html_body.as_deref().map(|html| {
        format!(
            "<p><br></p><p>---------- Forwarded message ----------<br>From: {}<br>Date: {}<br>Subject: {}<br>To: {}</p><blockquote>{html}</blockquote>",
            escape_html(&from),
            escape_html(&date),
            escape_html(original_subject),
            escape_html(&to),
        )
    });

    ComposePrefill {
        subject: Some(subject),
        body: Some(forward_body),
        html_body,
        ..Default::default()
    }
}

/// Builds the prefill for "Forward as attachment" (item 1.11 / C-13): a
/// `Fwd: `-prefixed subject, an empty body (the user types above the
/// attachment), and the whole original attached as a `message/rfc822` part
/// named from its subject. Unlike [`build_forward_prefill`] nothing is quoted
/// inline - the recipient opens the attached message to read it, which is
/// exactly how Outlook's "Forward as attachment" behaves.
pub fn build_forward_attachment_prefill(summary: &EmailSummary, raw: &[u8]) -> ComposePrefill {
    let subject = summary.subject.as_deref().map(|s| with_prefix_once(s, "Fwd: ")).unwrap_or_else(|| "Fwd: ".to_string());
    ComposePrefill {
        subject: Some(subject),
        body: Some("\n\n".to_string()),
        attachments: vec![Attachment {
            filename: crate::window::eml_suggested_name(summary),
            content_type: "message/rfc822".to_string(),
            bytes: raw.to_vec(),
        }],
        ..Default::default()
    }
}

/// Builds the prefill for reopening a saved draft (item 1.12), from the
/// envelope fields the reading pane already has (`summary`) plus the parsed
/// MIME content (`lookout_mail::body::parse_draft_prefill`). Unlike Reply,
/// nothing is filtered or rewritten: the draft is exactly what the user
/// saved, so its recipients, subject, bodies, threading, importance, receipt
/// request, attachments and identity are restored verbatim, and its
/// `(mailbox, uid)`/`Message-ID` ride along so the autosave replaces the
/// stored copy on save (see `ComposePrefill::draft_source`/`draft_message_id`).
pub fn build_draft_prefill(summary: &EmailSummary, content: &DraftPrefillContent) -> ComposePrefill {
    let recipients = |list: &[EmailAddress]| -> Option<String> {
        if list.is_empty() {
            return None;
        }
        Some(
            list.iter()
                .map(|a| match a.name.as_deref().filter(|name| !name.trim().is_empty()) {
                    Some(name) => format!("{name} <{}>", a.address),
                    None => a.address.clone(),
                })
                .collect::<Vec<_>>()
                .join(", "),
        )
    };
    ComposePrefill {
        to: recipients(&summary.to),
        cc: recipients(&summary.cc),
        bcc: (!content.bcc.is_empty()).then(|| content.bcc.join(", ")),
        subject: summary.subject.clone(),
        body: content.text_body.clone(),
        html_body: content.html_body.clone(),
        from: summary.from.first().map(|a| a.address.clone()),
        importance: summary.importance,
        sensitivity: content.sensitivity,
        request_read_receipt: content.request_read_receipt,
        attachments: content.attachments.clone(),
        // Defensive bracket strip: envelope-parsed ids are usually bare, but
        // a pre-parse fallback could carry the brackets.
        draft_message_id: summary.message_id.as_deref().map(strip_angle_brackets).filter(|id| !id.is_empty()).map(str::to_string),
        draft_source: Some((summary.mailbox.clone(), summary.uid)),
        draft_saved: false,
        in_reply_to: content.in_reply_to.clone(),
        references: content.references.clone(),
        calendar_part: None,
    }
}

/// Builds the prefill for reopening a message whose send was undone (item
/// 1.13): exactly what was about to go out - identity, recipients (Bcc
/// included), subject, bodies, attachments, importance, receipt request and
/// threading. The message is round-tripped through its draft MIME form
/// (`build_draft_raw_message` → `parse_draft_prefill`, as a reopened draft
/// is) so inline images come back as `data:` URIs the editor can show. Its
/// `Message-ID`, when set, names the autosaved draft still in Drafts (the
/// outbox only deletes it once the message is sent), so the reopened
/// composer keeps replacing that copy.
pub fn build_resend_prefill(msg: &ComposedMessage) -> ComposePrefill {
    let (raw, _, _) = lookout_mail::send::build_draft_raw_message(msg);
    let content = lookout_mail::body::parse_draft_prefill(&raw);
    let join = |list: &[String]| (!list.is_empty()).then(|| list.join(", "));
    let (body, html_body, attachments) = match content {
        Some(content) => (content.text_body, content.html_body, content.attachments),
        None => (Some(msg.text_body.clone()), None, msg.attachments.clone()),
    };
    ComposePrefill {
        to: join(&msg.to),
        cc: join(&msg.cc),
        bcc: join(&msg.bcc),
        subject: Some(msg.subject.clone()),
        body,
        html_body,
        from: Some(msg.from.clone()),
        importance: msg.importance,
        sensitivity: msg.sensitivity,
        request_read_receipt: msg.request_read_receipt,
        attachments,
        draft_message_id: msg.message_id.clone(),
        draft_saved: msg.message_id.is_some(),
        draft_source: None,
        in_reply_to: msg.in_reply_to.clone(),
        references: msg.references.clone(),
        calendar_part: None,
    }
}

/// Builds the prefill for reopening a send withdrawn from the outbox with no
/// in-memory copy to hand (item 1.14): one scheduled in an earlier run, or
/// cancelled from the scheduled-messages dialog or a draft's "Cancel send".
/// Everything comes from the queued raw message - the same parse a reopened
/// draft gets, plus the envelope from its headers - except the Bcc list,
/// which never travels in a sent message's headers and is carried by the
/// outbox payload instead. The `Message-ID` names the Drafts copy scheduling
/// saved, so the reopened composer keeps replacing it rather than adding a
/// second one. `None` when the stored bytes don't parse.
pub fn build_outbox_prefill(message: &lookout_mail::send::OutboxMessage) -> Option<ComposePrefill> {
    let raw = message.raw()?;
    let content = lookout_mail::body::parse_draft_prefill(&raw)?;
    let join = |list: &[String]| (!list.is_empty()).then(|| list.join(", "));
    Some(ComposePrefill {
        to: join(&content.to),
        cc: join(&content.cc),
        bcc: join(&message.bcc),
        subject: content.subject.clone().or_else(|| Some(message.subject.clone())),
        body: content.text_body,
        html_body: content.html_body,
        from: content.from.or_else(|| Some(message.from.clone())),
        importance: content.importance,
        sensitivity: content.sensitivity,
        request_read_receipt: content.request_read_receipt,
        attachments: content.attachments,
        draft_message_id: message.draft_message_id.clone(),
        draft_saved: message.draft_message_id.is_some(),
        draft_source: None,
        in_reply_to: content.in_reply_to,
        references: content.references,
        // An outbox-withdrawn iMIP message loses its calendar part: the raw
        // parse (`parse_draft_prefill`) deliberately excludes `text/calendar`.
        calendar_part: None,
    })
}

/// One of the Send menu's "Schedule send" presets (item 1.14).
#[derive(Debug, Clone, PartialEq)]
pub struct SchedulePreset<Tz: chrono::TimeZone> {
    pub name: &'static str,
    pub at: chrono::DateTime<Tz>,
}

/// New Outlook's schedule-send presets relative to `now`: tomorrow morning
/// (8:00), tomorrow afternoon (13:00) and Monday morning (8:00). Monday is
/// left out when it's tomorrow, since it would just repeat the first entry.
/// A local time that doesn't exist (a DST gap) resolves to its earliest
/// valid instant, or the preset is skipped.
pub fn schedule_presets<Tz: chrono::TimeZone>(now: &chrono::DateTime<Tz>) -> Vec<SchedulePreset<Tz>> {
    use chrono::{Datelike, Days, Weekday};
    let tz = now.timezone();
    let today = now.date_naive();
    let at = |date: chrono::NaiveDate, hour: u32| date.and_hms_opt(hour, 0, 0).and_then(|naive| naive.and_local_timezone(tz.clone()).earliest());
    let tomorrow = today + Days::new(1);
    let days_to_monday = (7 - today.weekday().num_days_from_monday()) % 7;
    let monday = today + Days::new(u64::from(if days_to_monday == 0 { 7 } else { days_to_monday }));
    let mut presets = Vec::new();
    let mut push = |name, at: Option<chrono::DateTime<Tz>>| {
        if let Some(at) = at {
            presets.push(SchedulePreset { name, at });
        }
    };
    push("Tomorrow morning", at(tomorrow, 8));
    push("Tomorrow afternoon", at(tomorrow, 13));
    if tomorrow.weekday() != Weekday::Mon {
        push("Monday morning", at(monday, 8));
    }
    presets
}

/// The Send split button's menu (item 1.14): the schedule-send presets,
/// recomputed every time the menu opens so "Tomorrow" never goes stale in a
/// long-open composer, then "Custom time…". `on_pick` receives the chosen
/// instant.
fn build_schedule_popover(on_pick: Rc<dyn Fn(chrono::DateTime<chrono::Utc>)>) -> gtk::Popover {
    crate::datetime_picker::build_time_preset_popover(
        "Schedule send",
        Rc::new(|now| {
            schedule_presets(&now)
                .into_iter()
                .map(|preset| crate::datetime_picker::TimeChoice {
                    name: preset.name.to_string(),
                    at: preset.at,
                })
                .collect()
        }),
        "Custom time…",
        crate::datetime_picker::TimeDialog {
            heading: "Schedule send",
            field_label: "Send at",
            accept: "Schedule",
            note: Some("Lookout must be running for the message to be sent."),
        },
        on_pick,
    )
}

/// `bcc` plus the identity's automatic Bcc addresses, skipping any already
/// listed (case-insensitively) - a reopened draft or undone send already
/// carries them from its first save, so they mustn't be added twice.
fn with_identity_bcc(mut bcc: Vec<String>, identity: &Identity) -> Vec<String> {
    for address in identity.bcc.iter().map(|a| a.address.clone()) {
        if !bcc.iter().any(|existing| existing.eq_ignore_ascii_case(&address)) {
            bcc.push(address);
        }
    }
    bcc
}

fn push_blockquote(out: &mut String, lines: &[String]) {
    out.push_str("<blockquote>");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("<br>");
        }
        out.push_str(&escape_html(line));
    }
    out.push_str("</blockquote>");
}

fn flush_paragraph(out: &mut String, lines: &mut Vec<String>) {
    if lines.is_empty() {
        return;
    }
    out.push_str("<p>");
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push_str("<br>");
        }
        out.push_str(&escape_html(line));
    }
    out.push_str("</p>");
    lines.clear();
}

/// Converts a plain-text body into simple HTML for seeding the rich editor:
/// special characters are escaped, `> `-prefixed lines are grouped into a
/// blockquote, `---`/`___`/`***` lines become a horizontal rule, blank lines
/// start a new paragraph and single newlines become `<br>`s. The plain-text
/// prefill is the source for the HTML mode too (the original message's HTML
/// is deliberately not reused for quoting), so both modes start from the
/// same content. Leading blank lines (before any real content) become
/// explicit empty paragraphs rather than being dropped, since the reply and
/// forward prefills lead with blank lines to leave room for the user to type
/// above the quoted content - CSS margins between `<p>` tags already cover
/// spacing *between* paragraphs, but there's nothing to put a margin next to
/// at the very top of the document.
fn text_to_html(text: &str) -> String {
    let mut out = String::new();
    let mut para_lines: Vec<String> = Vec::new();
    let mut quote_lines: Vec<String> = Vec::new();
    let mut seen_content = false;

    for raw in text.lines() {
        if let Some(rest) = raw.strip_prefix('>') {
            flush_paragraph(&mut out, &mut para_lines);
            quote_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
            seen_content = true;
            continue;
        }
        if !quote_lines.is_empty() {
            push_blockquote(&mut out, &quote_lines);
            quote_lines.clear();
        }
        let trimmed = raw.trim();
        if trimmed.len() >= 3 && trimmed.chars().all(|c| matches!(c, '-' | '_' | '*')) {
            flush_paragraph(&mut out, &mut para_lines);
            out.push_str("<hr>");
            seen_content = true;
        } else if trimmed.is_empty() {
            if seen_content {
                flush_paragraph(&mut out, &mut para_lines);
            } else {
                out.push_str("<p><br></p>");
            }
        } else {
            para_lines.push(raw.to_string());
            seen_content = true;
        }
    }
    if !quote_lines.is_empty() {
        push_blockquote(&mut out, &quote_lines);
    }
    flush_paragraph(&mut out, &mut para_lines);
    out
}

/// Wraps the editor's body HTML in a full document for `WebView::load_html`,
/// with a minimal stylesheet so quoted text stays visually distinct while
/// composing.
fn html_document(body: &str) -> String {
    format!(
        r#"<!DOCTYPE html><html><head><meta charset="utf-8"><style>
body {{ font-family: system-ui, sans-serif; font-size: 12pt; margin: 8px; }}
blockquote {{ margin: 4px 0 4px 16px; padding-left: 8px; border-left: 2px solid #999; color: #555; }}
</style></head><body>{body}</body></html>"#
    )
}

/// Builds the HTML for the "Insert table" toolbar button - `rows`/`cols`
/// come from trusted `gtk::SpinButton` values, not free text, so no
/// HTML-escaping is needed here.
fn table_html(rows: u32, cols: u32) -> String {
    let mut html = String::from(r#"<table style="border-collapse:collapse;width:100%">"#);
    for _ in 0..rows {
        html.push_str("<tr>");
        for _ in 0..cols {
            html.push_str(r#"<td style="border:1px solid #ccc;padding:4px;">&nbsp;</td>"#);
        }
        html.push_str("</tr>");
    }
    html.push_str("</table><p><br></p>");
    html
}

/// Reads the editor's content back out of the WebView with one
/// `evaluate_javascript` round trip, returning the normalized HTML and the
/// plain-text rendering. WebKit's `FontSize`/`ForeColor` editing commands
/// wrap their selection in `<font>` elements; the script rewrites those into
/// styled spans so the outgoing HTML is clean instead of full of obsolete
/// tags. Returns `None` if the evaluation fails (e.g. the document hasn't
/// finished loading), so callers can fall back to a known-good body rather
/// than dropping the message.
///
/// Must stay `async`. This used to `block_on` the evaluation to keep its
/// callers synchronous, which aborted the process the moment a caller ran
/// inside a glib-spawned future: `block_on` runs a *nested* main loop, that
/// loop dispatches another `TaskSource`, and its `futures_executor::enter()`
/// fails with `EnterError` because this thread already has an executor
/// entered for the outer dispatch. glib unwraps that, and `TaskSource::
/// dispatch` is an `extern "C"` callback that cannot unwind, so the panic
/// becomes an abort. Awaiting on the executor that's already running avoids
/// the nested loop entirely.
pub(crate) struct EditorContent {
    pub(crate) html: String,
    pub(crate) text: String,
    pub(crate) images: Vec<InlineImage>,
}

pub(crate) async fn read_content(web_view: &webkit::WebView) -> Option<EditorContent> {
    const READ_SCRIPT: &str = r#"(
        function () {
            document.querySelectorAll('font').forEach(function (el) {
                var span = document.createElement('span');
                var size = el.getAttribute('size');
                var color = el.getAttribute('color');
                if (size) {
                    var pt = size <= 1 ? 8 : size == 2 ? 10 : size == 3 ? 12 : size == 4 ? 14 : size == 5 ? 18 : size == 6 ? 24 : 36;
                    span.style.fontSize = pt + 'pt';
                }
                if (color) {
                    span.style.color = color;
                }
                while (el.firstChild) {
                    span.appendChild(el.firstChild);
                }
                el.parentNode.replaceChild(span, el);
            });
            // Inline images are held as `data:` URIs while composing (the
            // compose WebView has no `cid:` scheme handler, unlike the
            // reading pane's, so rewriting a *live* `data:` <img> to `cid:`
            // would make it vanish from the editor). Extraction therefore
            // runs on a clone, never the live document.
            var clone = document.body.cloneNode(true);
            var images = [];
            var seen = {};
            clone.querySelectorAll('img[src^="data:"]').forEach(function (img) {
                var m = /^data:([^;,]+)(;base64)?,(.*)$/s.exec(img.getAttribute('src'));
                if (!m || !m[2]) return;
                var contentType = m[1], data = m[3];
                var hash = 0;
                for (var i = 0; i < data.length; i++) { hash = ((hash << 5) - hash + data.charCodeAt(i)) | 0; }
                var cid = 'img-' + Math.abs(hash) + '@lookout.local';
                if (!seen[cid]) { seen[cid] = true; images.push({ cid: cid, contentType: contentType, data: data }); }
                img.setAttribute('src', 'cid:' + cid);
            });
            return JSON.stringify({ html: clone.innerHTML, text: document.body.innerText, images: images });
        }
    )()"#;

    let value = web_view.evaluate_javascript_future(READ_SCRIPT, None, None).await.ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&value.to_string()).ok()?;
    let html = parsed.get("html").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let text = parsed.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let images = parsed
        .get("images")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|entry| {
                    let cid = entry.get("cid")?.as_str()?.to_string();
                    let content_type = entry.get("contentType")?.as_str()?.to_string();
                    let bytes = base64::engine::general_purpose::STANDARD.decode(entry.get("data")?.as_str()?).ok()?;
                    Some(InlineImage { cid, content_type, bytes })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(EditorContent { html, text, images })
}

/// Adds a toolbar button that fires a plain WebKit editing command (bold,
/// italic, list, ...) on click. Built exactly like the main window's ribbon
/// command-toolbar buttons (`gtk::Button::from_icon_name` + tooltip), so the
/// rich-text icons carry the same background and dimensions as the ribbon
/// bar's default icons wherever the toolbar lives.
fn toolbar_command_button(toolbar: &gtk::Box, icon_name: &str, tooltip: &str, command: &'static str, web_view: &Rc<webkit::WebView>) {
    let button = gtk::Button::from_icon_name(icon_name);
    button.set_tooltip_text(Some(tooltip));
    let web_view = web_view.clone();
    button.connect_clicked(move |_| web_view.execute_editing_command(command));
    toolbar.append(&button);
}

/// Splits a recipient field's flattened text back into addresses.
///
/// Delegates to the chip widget's own tokenizer rather than splitting on
/// commas: a display name of the form `"Lovelace, Ada" <ada@example.com>`
/// carries a comma that is not a separator, and a draft autosave that split
/// it naively would file the draft with two broken recipients.
fn parse_recipients(field: &str) -> Vec<String> {
    crate::recipient_entry::parse_address_tokens(field)
}

/// Everything one draft-autosave needs to know to decide "did anything
/// change since the last save?" and to rebuild the message: the header rows
/// verbatim plus whichever body mode is live (`body` is the HTML in rich
/// mode and the plain text otherwise; `body_text` is always the plain-text
/// rendering).
#[derive(Clone, PartialEq)]
struct DraftSnapshot {
    /// The selected sending identity's address - a From change must re-save
    /// the draft like any other edit.
    from: String,
    to: String,
    cc: String,
    bcc: String,
    subject: String,
    rich: bool,
    /// Whether the "Request read receipt" switch is on - a flip re-saves the
    /// draft so the stored copy carries (or stops carrying) the
    /// `Disposition-Notification-To` header, like any other edit.
    read_receipt: bool,
    /// The composed message's Outlook importance (High/Normal/Low), from the
    /// toolbar's High/Low toggles - a flip re-saves the draft so it carries
    /// the matching `Importance`/`X-Priority` headers.
    importance: Importance,
    /// The composed message's RFC 2156 sensitivity, from the toolbar's
    /// dropdown - a change re-saves the draft so it carries (or drops) the
    /// `Sensitivity` header.
    sensitivity: MessageSensitivity,
    body: String,
    body_text: String,
    attachments: Vec<PendingAttachment>,
    inline_images: Vec<InlineImage>,
}

/// One file attached to the message being composed, held in memory for the
/// compose session. Drafts do persist attachments - they are MIME parts of
/// the saved message, and reopening a draft seeds this list from
/// `ComposePrefill::attachments` (item 1.12) - though a crash before the
/// next autosave still drops whatever was added since. `bytes` is
/// `Rc`-wrapped so add/remove/rebuild never reclones potentially multi-MB
/// payloads; it's cloned into an owned `lookout_mail::Attachment` only at
/// the two points that actually build a `ComposedMessage` (Send, and a
/// changed autosave tick).
#[derive(Clone)]
struct PendingAttachment {
    filename: String,
    content_type: String,
    bytes: Rc<Vec<u8>>,
}

impl PartialEq for PendingAttachment {
    // Identity, not content: two entries from the same pick share the same
    // `Rc`; a distinct pick (even of the same file) gets a fresh one. Keeps
    // `DraftSnapshot` equality (checked every autosave tick) cheap instead
    // of a multi-MB `memcmp`.
    fn eq(&self, other: &Self) -> bool {
        self.filename == other.filename && self.content_type == other.content_type && Rc::ptr_eq(&self.bytes, &other.bytes)
    }
}

impl PendingAttachment {
    fn to_mail_attachment(&self) -> Attachment {
        Attachment {
            filename: self.filename.clone(),
            content_type: self.content_type.clone(),
            bytes: (*self.bytes).clone(),
        }
    }
}

/// The identity the composer is currently set to send as: the dropdown's
/// selection, falling back to the first entry (the account's own default
/// identity), and ultimately to a blank placeholder - the send will be
/// rejected by the SMTP envelope check long before anything goes out.
fn selected_identity(identities: &Rc<RefCell<Vec<Identity>>>, from_dropdown: &gtk::DropDown) -> Identity {
    let list = identities.borrow();
    list.get(from_dropdown.selected() as usize)
        .cloned()
        .unwrap_or_else(|| list.first().cloned().unwrap_or_else(|| Identity::new(AccountId(String::new()), "", "")))
}

fn selected_importance(high: &gtk::ToggleButton, low: &gtk::ToggleButton) -> Importance {
    if high.is_active() {
        Importance::High
    } else if low.is_active() {
        Importance::Low
    } else {
        Importance::Normal
    }
}

/// The four RFC 2156 sensitivity labels, in dropdown order. Index 0 is
/// `Normal`; the order matches [`sensitivity_from_index`].
const SENSITIVITY_LABELS: [&str; 4] = ["Normal", "Personal", "Private", "Company-Confidential"];

fn selected_sensitivity(dropdown: &gtk::DropDown) -> MessageSensitivity {
    match dropdown.selected() {
        1 => MessageSensitivity::Personal,
        2 => MessageSensitivity::Private,
        3 => MessageSensitivity::CompanyConfidential,
        _ => MessageSensitivity::Normal,
    }
}

fn sensitivity_index(sensitivity: MessageSensitivity) -> u32 {
    match sensitivity {
        MessageSensitivity::Normal => 0,
        MessageSensitivity::Personal => 1,
        MessageSensitivity::Private => 2,
        MessageSensitivity::CompanyConfidential => 3,
    }
}

/// Everything the draft autosave touches, in one clonable bundle. This exists
/// because reading the rich editor is `async` (see `read_content`) and an
/// `Rc<dyn Fn()>` closure can't hold an `async` body - each caller clones a
/// context and awaits its methods instead.
#[derive(Clone)]
struct AutosaveCtx {
    to_row: RecipientEntry,
    cc_row: RecipientEntry,
    bcc_row: RecipientEntry,
    subject_row: adw::EntryRow,
    read_receipt_toggle: gtk::ToggleButton,
    high_importance_toggle: gtk::ToggleButton,
    low_importance_toggle: gtk::ToggleButton,
    sensitivity_dropdown: gtk::DropDown,
    rich_toggle: gtk::ToggleButton,
    body_view: gtk::TextView,
    rich_web_view: Rc<webkit::WebView>,
    /// The canonical in-session attachment list, shared with the attach
    /// button and the Send handler.
    attachments: Rc<RefCell<Vec<PendingAttachment>>>,
    /// The last snapshot queued for saving; a tick whose fields still match
    /// it does nothing.
    last_saved: Rc<RefCell<Option<DraftSnapshot>>>,
    /// Set the moment a `SaveDraft` is queued for this session - gates the
    /// `replace` flag and the delete-before-send, so it's deliberately
    /// optimistic (a queued-but-failed save still means "ask the server to
    /// replace / delete by Message-ID"; both are harmless no-ops if nothing
    /// is there).
    draft_queued: Rc<Cell<bool>>,
    /// Held for the duration of one save. Both the tick and Cancel can start
    /// a save, and each awaits a WebKit round trip in the middle - without
    /// this, two overlapping saves could both observe `draft_queued == false`
    /// and each `APPEND`, leaving two copies under one `Message-ID`.
    in_flight: Rc<Cell<bool>>,
    /// One stable Message-ID per compose session: every autosave carries it,
    /// and `replace` purges whatever the previous save stored under it, so
    /// the Drafts folder only ever holds one copy of this draft. A reopened
    /// draft seeds it with the saved message's own id.
    draft_message_id: Rc<String>,
    /// Set only when this composer reopened a saved draft (item 1.12): the
    /// source copy's exact `(mailbox, uid)`, taken by the *first* autosave
    /// so it can purge that copy directly (a foreign draft may carry no
    /// `Message-ID` to search by). Every later save purges by
    /// `draft_message_id` instead.
    replace_uid: Rc<RefCell<Option<(MailboxId, Uid)>>>,
    cmd_tx: async_channel::Sender<AccountCommand>,
    /// The account's selectable identities (default first) and the From
    /// dropdown that selects among them. Shared with the send handler, so
    /// whatever the dropdown shows is exactly what drafts and sends use.
    identities: Rc<RefCell<Vec<Identity>>>,
    from_dropdown: gtk::DropDown,
    in_reply_to: Option<String>,
    references: Vec<String>,
    /// The iMIP part an invitation reply carries (CAL-15); autosaved drafts
    /// keep it so a saved-then-sent reply is still a `METHOD:REPLY` part.
    calendar_part: Option<String>,
    status_label: gtk::Label,
}

impl AutosaveCtx {
    /// The composer's current contents, or `None` when the rich editor can't
    /// be read this tick (document still loading) - the next tick retries.
    async fn snapshot(&self) -> Option<DraftSnapshot> {
        let rich = self.rich_toggle.is_active();
        let (body, body_text, inline_images) = if rich {
            let content = read_content(&self.rich_web_view).await?;
            (content.html, content.text, content.images)
        } else {
            let buffer = self.body_view.buffer();
            let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
            (text.clone(), text, vec![])
        };
        Some(DraftSnapshot {
            from: selected_identity(&self.identities, &self.from_dropdown).email,
            to: self.to_row.text_value(),
            cc: self.cc_row.text_value(),
            bcc: self.bcc_row.text_value(),
            subject: self.subject_row.text().to_string(),
            rich,
            read_receipt: self.read_receipt_toggle.is_active(),
            importance: selected_importance(&self.high_importance_toggle, &self.low_importance_toggle),
            sensitivity: selected_sensitivity(&self.sensitivity_dropdown),
            body,
            body_text,
            attachments: self.attachments.borrow().clone(),
            inline_images,
        })
    }

    /// Saves the draft if anything worth saving has changed since the last
    /// save. Safe to call at any time: trivial, unchanged, and concurrent
    /// invocations all return without queueing anything.
    async fn attempt(&self) {
        if self.in_flight.get() {
            return;
        }
        self.in_flight.set(true);
        self.save_if_changed().await;
        self.in_flight.set(false);
    }

    /// Like `attempt`, but waits out a save already in flight first, so the
    /// final content is always queued - schedule send (item 1.14) relies on
    /// the Drafts copy being current, and on `draft_queued` being set,
    /// before it queues the message.
    async fn flush(&self) {
        while self.in_flight.get() {
            glib::timeout_future(Duration::from_millis(50)).await;
        }
        self.attempt().await;
    }

    async fn save_if_changed(&self) {
        let Some(snap) = self.snapshot().await else { return };
        if draft_is_trivial(&snap) {
            return;
        }
        // Scoped so no borrow is held across the sends below - and, more to
        // the point, never across the `.await` above, which yields to the
        // main loop and lets other handlers run.
        let unchanged = self.last_saved.borrow().as_ref() == Some(&snap);
        if unchanged {
            return;
        }
        // First save of a reopened draft takes its exact source UID (then
        // clears it); every other replace works by Message-ID, which by now
        // names either this session's earlier save or the reopened draft's
        // own id.
        let replace = if !self.draft_queued.get() {
            DraftReplace::None
        } else if let Some((mailbox, uid)) = self.replace_uid.borrow_mut().take() {
            DraftReplace::ByUid { mailbox, uid }
        } else {
            DraftReplace::ByMessageId
        };
        *self.last_saved.borrow_mut() = Some(snap.clone());
        self.draft_queued.set(true);
        let identity = selected_identity(&self.identities, &self.from_dropdown);
        let bcc = with_identity_bcc(parse_recipients(&snap.bcc), &identity);
        let msg = ComposedMessage {
            from: identity.email,
            display_name: (!identity.name.trim().is_empty()).then(|| identity.name.clone()),
            to: parse_recipients(&snap.to),
            cc: parse_recipients(&snap.cc),
            bcc,
            reply_to: identity.reply_to.iter().map(|a| a.address.clone()).collect(),
            subject: snap.subject.clone(),
            text_body: snap.body_text.clone(),
            html_body: snap.rich.then(|| snap.body.clone()),
            attachments: snap.attachments.iter().map(PendingAttachment::to_mail_attachment).collect(),
            inline_images: snap.inline_images.clone(),
            calendar_part: self.calendar_part.clone(),
            read_receipt: None,
            request_read_receipt: snap.read_receipt,
            importance: snap.importance,
            sensitivity: snap.sensitivity,
            in_reply_to: self.in_reply_to.clone(),
            references: self.references.clone(),
            message_id: Some((*self.draft_message_id).clone()),
        };
        let _ = self.cmd_tx.send_blocking(AccountCommand::SaveDraft { msg: Box::new(msg), replace });
        self.status_label.set_label("Saving draft…");
    }
}

/// A draft with no recipients, no subject and no body content is not worth
/// storing - an untouched "New Message" composer must not litter the Drafts
/// folder with empty messages. Whitespace-only counts as empty; the rich
/// editor's blank document (`<p><br></p>`) renders to whitespace-only text,
/// so it's caught by the same check.
fn draft_is_trivial(snap: &DraftSnapshot) -> bool {
    snap.to.trim().is_empty()
        && snap.cc.trim().is_empty()
        && snap.bcc.trim().is_empty()
        && snap.subject.trim().is_empty()
        && snap.body_text.trim().is_empty()
        && snap.attachments.is_empty()
        && snap.inline_images.is_empty()
}

/// Word stems that suggest the sender meant to attach something (item 1.19 /
/// C-16). Matched against whole tokens, so "attach" catches "attached",
/// "attachment" and "attachments" but not "detached". Localisable: add each
/// language's stems here.
const ATTACHMENT_REMINDER_STEMS: &[&str] = &["attach", "enclos"];

/// Whether `text` mentions an attachment, per [`ATTACHMENT_REMINDER_STEMS`].
fn mentions_attachment(text: &str) -> bool {
    text.split(|c: char| !c.is_alphanumeric()).filter(|token| !token.is_empty()).any(|token| {
        let token = token.to_ascii_lowercase();
        ATTACHMENT_REMINDER_STEMS.iter().any(|stem| token.starts_with(stem))
    })
}

/// Whether sending a message with `subject` and `body` but no attachments
/// should prompt first (item 1.19 / C-16): the text mentions an attachment
/// and none is attached. Pure, so it's unit-tested.
fn looks_like_missing_attachment(subject: &str, body: &str) -> bool {
    mentions_attachment(subject) || mentions_attachment(body)
}

thread_local! {
    /// The composer's own WebKit context, kept separate from the process-wide
    /// default so enabling spellchecking here never affects the reading pane,
    /// the assistant or the print pipeline. Created once and shared by every
    /// composer and the signature editor.
    static COMPOSE_WEB_CONTEXT: RefCell<Option<webkit::WebContext>> = const { RefCell::new(None) };
}

/// The shared composer `WebContext`, created on first use.
pub(crate) fn compose_web_context() -> webkit::WebContext {
    COMPOSE_WEB_CONTEXT.with(|slot| slot.borrow_mut().get_or_insert_with(webkit::WebContext::new).clone())
}

/// Turns spellchecking on or off on the composer's shared context and sets the
/// languages to check. `configured_languages` are `en_US`-style tags; empty
/// means the system locale's language.
pub(crate) fn apply_spell_check(enabled: bool, configured_languages: &[String]) {
    let context = compose_web_context();
    context.set_spell_checking_enabled(enabled);
    let languages = resolved_spell_languages(configured_languages);
    if !languages.is_empty() {
        let refs: Vec<&str> = languages.iter().map(String::as_str).collect();
        context.set_spell_checking_languages(&refs);
    }
}

/// The language tags to spellcheck in: the configured ones, or, when none are
/// configured, the system locale's first non-`C`/`POSIX` language.
fn resolved_spell_languages(configured: &[String]) -> Vec<String> {
    if !configured.is_empty() {
        return configured.iter().filter(|tag| !tag.is_empty()).cloned().collect();
    }
    glib::language_names()
        .into_iter()
        .map(|tag| tag.to_string())
        .find(|tag| !tag.eq_ignore_ascii_case("c") && !tag.eq_ignore_ascii_case("posix"))
        .into_iter()
        .collect()
}

/// Builds the rich-text editor's pieces: the contenteditable WebKit `WebView`
/// plus its formatting toolbar (bold, italic, lists, font size, color,
/// link). The caller composes the toolbar row (adding the plain/rich toggle
/// ahead of these buttons) and the scroller around the view. The whole page
/// behaves as a contenteditable document, so formatting actions map to
/// WebKit editing commands (`Bold`, `InsertUnorderedList`, `FontSize`,
/// `ForeColor`, `CreateLink`, ...) and the final content is read back with
/// `evaluate_javascript` at send time (see `read_content`). Navigation and
/// remote subresources are vetoed regardless of the reading pane's "Load
/// images" setting - composing must never fetch remote content. Returns the
/// WebView (which the caller keeps alive for send-time content reads) plus
/// the toolbar.
pub(crate) fn build_rich_editor(initial_html: String) -> (Rc<webkit::WebView>, gtk::Box) {
    let settings = webkit::Settings::builder()
        .enable_javascript(true)
        .hardware_acceleration_policy(webkit::HardwareAccelerationPolicy::Never)
        .build();
    let web_view = Rc::new(webkit::WebView::builder().editable(true).settings(&settings).web_context(&compose_web_context()).build());
    // The draft body is editable HTML; "All text" mode blurs the whole view.
    crate::privacy::mark_blur_all(web_view.as_ref());
    // Ctrl+scroll / Ctrl+± zoom on the composer (item 1.20 / M-37).
    crate::window::install_web_view_zoom(&web_view);

    {
        let web_view = web_view.clone();
        web_view.connect_decide_policy(move |_view, decision, decision_type| {
            let uri_is_local = |uri: &str| matches!(uri.split(':').next().unwrap_or(""), "data" | "cid" | "about" | "file");
            match decision_type {
                webkit::PolicyDecisionType::NavigationAction => {
                    let navigation = decision.downcast_ref::<webkit::NavigationPolicyDecision>().and_then(|d| d.navigation_action());
                    if let Some(uri) = navigation.as_ref().and_then(|a| a.request()).and_then(|r| r.uri()) {
                        if !uri_is_local(&uri) {
                            decision.ignore();
                            return true;
                        }
                    }
                }
                webkit::PolicyDecisionType::Response => {
                    if let Some(response) = decision.downcast_ref::<webkit::ResponsePolicyDecision>() {
                        if !response.is_main_frame_main_resource() {
                            if let Some(uri) = response.request().and_then(|r| r.uri()) {
                                if !uri_is_local(&uri) {
                                    decision.ignore();
                                    return true;
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            false
        });
    }

    let initial_body = if initial_html.trim().is_empty() { "<p><br></p>".to_string() } else { initial_html };
    web_view.load_html(&html_document(&initial_body), None);

    // A pasted image becomes an inline `data:` <img> (extracted back out to
    // a `cid:` attachment at read time - see `read_content`). WebKitGTK's
    // default paste behavior for clipboard image data in a contenteditable
    // document isn't relied on here - this listener owns the whole path
    // explicitly, guarded so a second `load_html` (this composer never
    // re-loads, but this stays safe if that ever changes) doesn't double up.
    // Drag-and-drop image insertion is out of scope: dropping is prevented
    // outright rather than left to an unverified WebKit default (which could
    // otherwise navigate the frame to a dropped file).
    {
        let web_view = web_view.clone();
        web_view.connect_load_changed(move |view, event| {
            if event != webkit::LoadEvent::Finished {
                return;
            }
            const PASTE_SCRIPT: &str = r#"(function () {
                if (window.__lookoutPasteHandlerInstalled) return;
                window.__lookoutPasteHandlerInstalled = true;
                document.body.addEventListener('paste', function (e) {
                    var items = e.clipboardData && e.clipboardData.items;
                    if (!items) return;
                    for (var i = 0; i < items.length; i++) {
                        if (items[i].type.indexOf('image/') !== 0) continue;
                        e.preventDefault();
                        var file = items[i].getAsFile();
                        var reader = new FileReader();
                        reader.onload = function () {
                            document.execCommand('insertHTML', false, '<img src="' + reader.result + '" alt="">');
                        };
                        reader.readAsDataURL(file);
                        return;
                    }
                });
                document.body.addEventListener('dragover', function (e) { e.preventDefault(); });
                document.body.addEventListener('drop', function (e) { e.preventDefault(); });
            })()"#;
            let view = view.clone();
            glib::spawn_future_local(async move {
                let _ = view.evaluate_javascript_future(PASTE_SCRIPT, None, None).await;
            });
        });
    }

    let toolbar = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(4).build();
    toolbar_command_button(&toolbar, "format-text-bold-symbolic", "Bold", "Bold", &web_view);
    toolbar_command_button(&toolbar, "format-text-italic-symbolic", "Italic", "Italic", &web_view);
    toolbar_command_button(&toolbar, "format-text-underline-symbolic", "Underline", "Underline", &web_view);
    toolbar_command_button(&toolbar, "format-text-strikethrough-symbolic", "Strikethrough", "StrikeThrough", &web_view);
    toolbar.append(&gtk::Separator::builder().orientation(gtk::Orientation::Vertical).build());
    toolbar_command_button(&toolbar, "list-bullet-symbolic", "Bulleted list", "InsertUnorderedList", &web_view);
    toolbar_command_button(&toolbar, "list-number-symbolic", "Numbered list", "InsertOrderedList", &web_view);

    let font_sizes = gtk::StringList::new(&["Small", "Normal", "Large", "Huge"]);
    let font_size_args = ["2", "3", "5", "7"];
    let font_size = gtk::DropDown::builder().model(&font_sizes).selected(1).tooltip_text("Font size").build();
    {
        let web_view = web_view.clone();
        font_size.connect_selected_notify(move |dropdown| {
            if let Some(arg) = font_size_args.get(dropdown.selected() as usize) {
                web_view.execute_editing_command_with_argument("FontSize", arg);
            }
        });
    }
    toolbar.append(&font_size);

    let color_dialog = gtk::ColorDialog::new();
    let color_button = gtk::ColorDialogButton::builder().dialog(&color_dialog).tooltip_text("Text color").build();
    {
        let web_view = web_view.clone();
        color_button.connect_rgba_notify(move |button| {
            let rgba = button.rgba();
            let hex = format!(
                "#{:02x}{:02x}{:02x}",
                (rgba.red() * 255.0).round() as u8,
                (rgba.green() * 255.0).round() as u8,
                (rgba.blue() * 255.0).round() as u8
            );
            web_view.execute_editing_command_with_argument("ForeColor", &hex);
        });
    }
    toolbar.append(&color_button);

    let link_entry = gtk::Entry::builder().placeholder_text("https://example.com").build();
    let link_dialog = adw::AlertDialog::builder()
        .heading("Insert link")
        .body("Enter the destination URL")
        .default_response("insert")
        .close_response("cancel")
        .build();
    link_dialog.add_response("cancel", "Cancel");
    link_dialog.add_response("insert", "Insert");
    link_dialog.set_response_appearance("insert", adw::ResponseAppearance::Suggested);
    link_dialog.set_extra_child(Some(&link_entry));
    let link_dialog = Rc::new(link_dialog);
    let link_entry = Rc::new(link_entry);
    {
        let web_view = web_view.clone();
        link_dialog.connect_response(None, move |_dialog, response| {
            let url = link_entry.text().to_string();
            link_entry.set_text("");
            if response == "insert" && !url.is_empty() {
                web_view.execute_editing_command_with_argument("CreateLink", &url);
            }
        });
    }
    let link_button = gtk::Button::from_icon_name("insert-link-symbolic");
    link_button.set_tooltip_text(Some("Insert link"));
    link_button.connect_clicked(move |button| link_dialog.present(Some(button)));
    toolbar.append(&link_button);

    // Emoji picker (item 1.20 / C-14): a MenuButton wrapping a
    // `gtk::EmojiChooser`; picking inserts the emoji at the caret.
    let emoji_button = gtk::MenuButton::builder().icon_name("face-smile-symbolic").tooltip_text("Insert emoji").build();
    let emoji_chooser = gtk::EmojiChooser::new();
    {
        let web_view = web_view.clone();
        emoji_chooser.connect_emoji_picked(move |_chooser, emoji| {
            // The picker held focus; hand it back before inserting so the
            // emoji lands at the caret rather than nowhere.
            web_view.grab_focus();
            web_view.execute_editing_command_with_argument("InsertText", emoji);
        });
    }
    emoji_button.set_popover(Some(&emoji_chooser));
    toolbar.append(&emoji_button);

    toolbar.append(&gtk::Separator::builder().orientation(gtk::Orientation::Vertical).build());
    toolbar_command_button(&toolbar, "format-justify-left-symbolic", "Align left", "JustifyLeft", &web_view);
    toolbar_command_button(&toolbar, "format-justify-center-symbolic", "Center", "JustifyCenter", &web_view);
    toolbar_command_button(&toolbar, "format-justify-right-symbolic", "Align right", "JustifyRight", &web_view);
    toolbar_command_button(&toolbar, "format-justify-fill-symbolic", "Justify", "JustifyFull", &web_view);
    toolbar_command_button(&toolbar, "format-indent-more-symbolic", "Indent", "Indent", &web_view);
    toolbar_command_button(&toolbar, "format-indent-less-symbolic", "Outdent", "Outdent", &web_view);
    toolbar_command_button(&toolbar, "insert-hr-symbolic", "Horizontal rule", "InsertHorizontalRule", &web_view);
    toolbar_command_button(&toolbar, "edit-clear-all-symbolic", "Remove formatting", "RemoveFormat", &web_view);

    let font_families = gtk::StringList::new(&["Sans-serif", "Serif", "Monospace", "Cursive"]);
    let font_family_args = ["sans-serif", "serif", "monospace", "cursive"];
    let font_family = gtk::DropDown::builder().model(&font_families).selected(0).tooltip_text("Font family").build();
    {
        let web_view = web_view.clone();
        font_family.connect_selected_notify(move |dropdown| {
            if let Some(arg) = font_family_args.get(dropdown.selected() as usize) {
                web_view.execute_editing_command_with_argument("FontName", arg);
            }
        });
    }
    toolbar.append(&font_family);

    let table_rows = gtk::SpinButton::with_range(1.0, 20.0, 1.0);
    table_rows.set_value(2.0);
    let table_cols = gtk::SpinButton::with_range(1.0, 10.0, 1.0);
    table_cols.set_value(2.0);
    let table_grid = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(8).build();
    let rows_label = gtk::Label::new(Some("Rows"));
    crate::privacy::mark_ui(&rows_label);
    table_grid.append(&rows_label);
    table_grid.append(&table_rows);
    let columns_label = gtk::Label::new(Some("Columns"));
    crate::privacy::mark_ui(&columns_label);
    table_grid.append(&columns_label);
    table_grid.append(&table_cols);
    let table_dialog = adw::AlertDialog::builder()
        .heading("Insert table")
        .default_response("insert")
        .close_response("cancel")
        .build();
    table_dialog.add_response("cancel", "Cancel");
    table_dialog.add_response("insert", "Insert");
    table_dialog.set_response_appearance("insert", adw::ResponseAppearance::Suggested);
    table_dialog.set_extra_child(Some(&table_grid));
    let table_dialog = Rc::new(table_dialog);
    {
        let web_view = web_view.clone();
        let table_rows = table_rows.clone();
        let table_cols = table_cols.clone();
        table_dialog.connect_response(None, move |_dialog, response| {
            if response == "insert" {
                let html = table_html(table_rows.value() as u32, table_cols.value() as u32);
                web_view.execute_editing_command_with_argument("InsertHTML", &html);
            }
        });
    }
    let table_button = gtk::Button::from_icon_name("view-grid-symbolic");
    table_button.set_tooltip_text(Some("Insert table"));
    table_button.connect_clicked(move |button| table_dialog.present(Some(button)));
    toolbar.append(&table_button);

    let insert_image_button = gtk::Button::from_icon_name("insert-image-symbolic");
    insert_image_button.set_tooltip_text(Some("Insert image"));
    {
        let web_view = web_view.clone();
        insert_image_button.connect_clicked(move |button| {
            let Some(window) = button.root().and_downcast::<gtk::Window>() else { return };
            let web_view = web_view.clone();
            glib::spawn_future_local(async move {
                let filter = gtk::FileFilter::new();
                filter.add_pixbuf_formats();
                let filters = gio::ListStore::new::<gtk::FileFilter>();
                filters.append(&filter);
                let dialog = gtk::FileDialog::builder().title("Insert image").filters(&filters).build();
                let Ok(file) = dialog.open_future(Some(&window)).await else { return };
                let Ok(info) = file
                    .query_info_future("standard::content-type", gio::FileQueryInfoFlags::NONE, glib::Priority::DEFAULT)
                    .await
                else {
                    return;
                };
                let content_type = info
                    .content_type()
                    .and_then(|ct| gio::functions::content_type_get_mime_type(&ct))
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "image/png".to_string());
                let Ok((bytes, _etag)) = file.load_contents_future().await else { return };
                let data_uri = format!("data:{content_type};base64,{}", base64::engine::general_purpose::STANDARD.encode(&bytes));
                web_view.grab_focus();
                // `grab_focus` only gives the WebView widget keyboard focus - it
                // doesn't put a caret in `document.body`, which is what the
                // insert command actually targets. If the user hasn't clicked
                // into the body yet there's no selection to insert at, so one
                // is created here (collapsed to the end of the content) before
                // firing the insert.
                let script = format!(
                    r#"(function () {{
                        document.body.focus();
                        var sel = window.getSelection();
                        var inBody = sel.rangeCount > 0 && document.body.contains(sel.getRangeAt(0).commonAncestorContainer);
                        if (!inBody) {{
                            var r = document.createRange();
                            r.selectNodeContents(document.body);
                            r.collapse(false);
                            sel.removeAllRanges();
                            sel.addRange(r);
                        }}
                        document.execCommand('insertImage', false, {data_uri_json});
                    }})()"#,
                    data_uri_json = serde_json::to_string(&data_uri).unwrap()
                );
                let _ = web_view.evaluate_javascript_future(&script, None, None).await;
            });
        });
    }
    toolbar.append(&insert_image_button);

    (web_view, toolbar)
}

/// Rebuilds `flow`'s attachment chips from `attachments`' current contents -
/// the same repopulate-from-truth shape `RecipientEntry` uses for its
/// recipient chips - and toggles the flow's visibility on empty/non-empty so
/// it collapses out of the layout when there's nothing attached.
fn rebuild_attachment_chips(attachments: &Rc<RefCell<Vec<PendingAttachment>>>, flow: &gtk::FlowBox) {
    while let Some(child) = flow.first_child() {
        flow.remove(&child);
    }
    for (index, item) in attachments.borrow().iter().enumerate() {
        let chip = build_attachment_chip(attachments, flow, index, item);
        flow.insert(&chip, index as i32);
    }
    flow.set_visible(!attachments.borrow().is_empty());
}

/// Reads `file` and appends it to `attachments` as a pending outgoing
/// attachment - the shared body of the Attach button and the composer's file
/// drop target (item 1.20 / C-15). A file whose info or contents can't be read
/// is skipped.
async fn add_attachment_from_file(file: &gio::File, attachments: &Rc<RefCell<Vec<PendingAttachment>>>) {
    let Ok(info) = file
        .query_info_future("standard::display-name,standard::content-type", gio::FileQueryInfoFlags::NONE, glib::Priority::DEFAULT)
        .await
    else {
        return;
    };
    let content_type = info
        .content_type()
        .and_then(|ct| gio::functions::content_type_get_mime_type(&ct))
        .map(|s| s.to_string())
        .unwrap_or_else(|| "application/octet-stream".to_string());
    let Ok((bytes, _etag)) = file.load_contents_future().await else {
        return;
    };
    attachments.borrow_mut().push(PendingAttachment {
        filename: info.display_name().to_string(),
        content_type,
        bytes: Rc::new(bytes.to_vec()),
    });
}

/// Installs a drop target on `widget` that turns dropped files into
/// attachments through the same [`add_attachment_from_file`] path as the
/// Attach button (item 1.20 / C-15). Dropping a file from the file manager
/// onto the composer body is Outlook's drag-in gesture.
fn install_file_drop_target(widget: &impl IsA<gtk::Widget>, attachments: Rc<RefCell<Vec<PendingAttachment>>>, flow: gtk::FlowBox) {
    let target = gtk::DropTarget::new(gtk::gdk::FileList::static_type(), gtk::gdk::DragAction::COPY);
    target.connect_drop(move |_target, value, _x, _y| {
        let Ok(files) = value.get::<gtk::gdk::FileList>() else {
            return false;
        };
        let attachments = attachments.clone();
        let flow = flow.clone();
        glib::spawn_future_local(async move {
            for file in files.files() {
                add_attachment_from_file(&file, &attachments).await;
            }
            rebuild_attachment_chips(&attachments, &flow);
        });
        true
    });
    widget.add_controller(target);
}

fn build_attachment_chip(attachments: &Rc<RefCell<Vec<PendingAttachment>>>, flow: &gtk::FlowBox, index: usize, item: &PendingAttachment) -> gtk::Box {
    let label = gtk::Label::builder()
        .label(format!("{}  ·  {}", item.filename, lookout_core::text::human_size(item.bytes.len() as u64)))
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .max_width_chars(28)
        .build();
    let remove = gtk::Button::builder()
        .icon_name("window-close-symbolic")
        .css_classes(["flat", "circular", "recipient-chip-remove"])
        .tooltip_text("Remove attachment")
        .build();
    {
        let attachments = attachments.clone();
        let flow = flow.clone();
        remove.connect_clicked(move |_| {
            // Read the index at click time from the position the chip was
            // built at: a rebuild recreates every chip, so a handler can
            // never outlive its own row.
            let mut list = attachments.borrow_mut();
            if index < list.len() {
                list.remove(index);
            }
            drop(list);
            rebuild_attachment_chips(&attachments, &flow);
        });
    }

    let chip = gtk::Box::builder().orientation(gtk::Orientation::Horizontal).spacing(2).build();
    chip.add_css_class("recipient-chip");
    chip.set_tooltip_text(Some(&item.filename));
    chip.append(&label);
    chip.append(&remove);
    chip
}

/// Builds a composer widget, meant to be embedded as a page in the reading
/// pane's `gtk::Stack` (see `window.rs`'s
/// `show_composer_in_reading_pane`) rather than presented as its own window.
/// `on_done` is called once Cancel or Send is clicked, so the caller can
/// tear the page down and restore whatever the reading pane showed before.
///
/// The body supports two modes, switched with the "Rich text" toggle in the
/// editor toolbar (the same row as the formatting buttons, which only make
/// sense in rich mode): the plain `gtk::TextView` fallback (also a send-time
/// fallback for HTML clients), and a contenteditable `WebKit.WebView` with a
/// formatting toolbar (see `build_rich_editor`). Only one mode is live at a
/// time; `rich_text_default` picks which one the composer opens on (Config →
/// Mail). The HTML mode sends `multipart/alternative` - both the rich HTML
/// and a plain-text rendering - so recipients without HTML support still get
/// the text.
///
/// Draft autosave: while the composer is open, a periodic timer compares
/// the fields against the last saved snapshot and, when they differ (and
/// the content isn't trivial), `APPEND`s the message to the account's
/// Drafts mailbox via `AccountCommand::SaveDraft` - replacing the previous
/// autosave in place, keyed by a stable per-compose-session `Message-ID`.
/// Cancel saves one final time (so closing the composer never loses work);
/// Send deletes the stored draft before sending. The returned `Sender` is
/// how the account's event loop forwards `DraftSaved` confirmations back to
/// this composer (which flips the status label from "Saving draft…" to
/// "Draft saved"); dropping it lets the composer's confirmation consumer
/// exit. The returned refresh hook re-reads `identities_source` into the
/// From dropdown - the caller invokes it after the Config → Mail accounts
/// manage-identities dialog changes the account's identities, so an edit
/// made while a composer is open shows up in its From list immediately.
///
/// `on_pop_out`, when provided, arms the header's pop-out button: clicking
/// it hands the composer (still fully alive - draft autosave included) to
/// the caller via `PopOutHandle`, which is expected to move it into its own
/// window. The composer sets the handle's `moving`/`popped_out` flags for
/// the duration of the move so its autosave isn't mistaken for a displaced
/// composer, and hides the button while popped out; the window layer flips
/// the same flags when moving it back and re-shows it via `root-notify`.
/// Cohesive arguments (they all describe one composer to open), so they stay
/// positional rather than being bundled into a single-use struct.
///
/// Returns the composer root (`content`), the editor toolbar row
/// (`editor_toolbar`), the draft-confirmation relay, and the identities-
/// refresh hook. The editor toolbar is built inside `content` but the
/// caller (`show_composer_in_reading_pane` in `window.rs`) relocates it to
/// the main window's action bar while the composer is embedded, and the
/// pop-out/pop-back-in handlers move it between the composer and that bar
/// (see `ComposeActionBar`); `editor_toolbar` is returned separately so the
/// caller can reparent it without reaching into the composer's internals.
#[allow(clippy::too_many_arguments)]
pub fn build_compose_view(
    title: &str,
    identities_source: Rc<dyn Fn() -> Vec<Identity>>,
    signatures_source: Rc<dyn Fn() -> Vec<Signature>>,
    manage_signatures: Option<Rc<dyn Fn()>>,
    // The global "My Templates" list (item 2.10 / C-17) and the callback that
    // jumps to the template manager in Settings. The composer's template menu
    // reads the source live on every open, like the signature menu.
    templates_source: Rc<dyn Fn() -> Vec<MailTemplate>>,
    manage_templates: Option<Rc<dyn Fn()>>,
    // The account's default signature for this compose kind (new message or
    // reply/forward), resolved by the caller from `AppConfig::signature_defaults`.
    // Appended to the end of the body when the composer opens.
    default_signature: Option<Signature>,
    cmd_tx: async_channel::Sender<AccountCommand>,
    prefill: ComposePrefill,
    on_done: Rc<dyn Fn()>,
    rich_text_default: bool,
    // Whether to spellcheck as the user types, and the `en_US`-style language
    // tags to check (empty = the system locale's language). Applied to the
    // composer's shared WebKit context (see `apply_spell_check`).
    spell_check: bool,
    spell_languages: Vec<String>,
    // Whether sending a message that mentions an attachment, but has none,
    // prompts first (item 1.19 / C-16).
    warn_no_attachment: bool,
    suggestions: SuggestionSource,
    open_contact_picker: crate::contact_picker::ContactPickerOpener,
    on_pop_out: Option<Rc<dyn Fn(PopOutHandle)>>,
    on_send_started: SendStarted,
    // Server-side schedule send (item 5.5 / C-08): returns true when the
    // message was scheduled through Exchange (so nothing goes in the local
    // outbox), false to fall back to it.
    on_schedule_send: Option<ScheduleSendHook>,
    // Whether to move the composer into its own window straight away
    // (CAL-15's "Edit response before sending"): the same pop-out path the
    // header button runs, scheduled on the next main-loop turn so the
    // composer is rooted in its reading-pane page first.
    pop_out_on_open: bool,
    // The shared ribbon layout state (`ribbon.rs`), so the composer's command
    // toolbar dresses (and re-dresses) its controls exactly like the main
    // window's ribbon.
    ribbon_style: crate::ribbon::RibbonStyle,
) -> (gtk::Box, gtk::Box, async_channel::Sender<String>, Rc<dyn Fn()>) {
    // Chip-mode fields (UX-42): each recipient is a removable pill, so a
    // contact list picked from the address book can render as a single
    // expandable list chip (P-05). The To row has no caption label of its
    // own: the To button in its header row (below) both labels the row and
    // opens the address-book picker.
    let to_row = RecipientEntry::new("");
    if let Some(to) = &prefill.to {
        to_row.set_from_text(to);
    }
    let cc_row = RecipientEntry::new("Cc");
    let cc_prefilled = prefill.cc.as_ref().is_some_and(|cc| !cc.trim().is_empty());
    if let Some(cc) = &prefill.cc {
        cc_row.set_from_text(cc);
    }
    // Bcc is only ever prefilled when reopening a saved draft; Reply and
    // Forward can't know a blind-copy list by definition.
    let bcc_row = RecipientEntry::new("Bcc");
    let bcc_prefilled = prefill.bcc.as_ref().is_some_and(|bcc| !bcc.trim().is_empty());
    if let Some(bcc) = &prefill.bcc {
        bcc_row.set_from_text(bcc);
    }
    for row in [&to_row, &cc_row, &bcc_row] {
        row.set_suggestion_source(suggestions.clone());
    }

    // --- To/Cc/Bcc buttons: To leads the row (it doubles as the field's
    // label, since the plain-line To field has no caption of its own) and
    // opens the address-book picker; Cc/Bcc trail on the right, Gmail's
    // classic reveal links, each hiding itself and showing its row once
    // clicked. A row that already has prefilled content (e.g. Reply All's
    // Cc list) starts shown, with its button skipped so there's nothing to
    // click for a field the user can already see.
    let to_button = gtk::Button::builder()
        .label("To")
        .css_classes(["flat"])
        .tooltip_text("Add recipients from address book")
        .build();
    let cc_button = gtk::Button::builder().label("Cc").css_classes(["flat", "caption"]).tooltip_text("Show Cc").build();
    let bcc_button = gtk::Button::builder().label("Bcc").css_classes(["flat", "caption"]).tooltip_text("Show Bcc").build();
    to_row.add_header_suffix(&to_button);
    to_row.add_header_suffix(&cc_button);
    to_row.add_header_suffix(&bcc_button);
    {
        let to_row = to_row.clone();
        to_button.connect_clicked(move |button| {
            let on_add = {
                let to_row = to_row.clone();
                Rc::new(move |picked: Vec<Suggestion>| to_row.append_suggestions(&picked))
            };
            open_contact_picker(button.clone().upcast(), on_add);
        });
    }
    cc_row.widget().set_visible(cc_prefilled);
    cc_button.set_visible(!cc_prefilled);
    bcc_row.widget().set_visible(bcc_prefilled);
    bcc_button.set_visible(!bcc_prefilled);
    {
        let cc_row = cc_row.clone();
        let cc_button_self = cc_button.clone();
        cc_button.connect_clicked(move |_| {
            cc_row.widget().set_visible(true);
            cc_button_self.set_visible(false);
        });
    }
    {
        let bcc_row = bcc_row.clone();
        let bcc_button_self = bcc_button.clone();
        bcc_button.connect_clicked(move |_| {
            bcc_row.widget().set_visible(true);
            bcc_button_self.set_visible(false);
        });
    }
    let subject_row = adw::EntryRow::builder().title("Subject").build();
    if let Some(subject) = &prefill.subject {
        subject_row.set_text(subject);
    }

    // --- From selector: a dropdown of the account's identities (its own
    // address first), living in the header row next to Send rather than as
    // its own field row. The returned refresh hook re-reads
    // `identities_source` when the Config → Mail accounts manage dialog
    // changes them, so an edit while a composer is open lands in its From
    // list immediately.
    let identities = Rc::new(RefCell::new(identities_source()));
    let from_dropdown = gtk::DropDown::builder().selected(0).build();
    // Identity names/addresses are content: blurred by privacy mode.
    crate::privacy::mark_content(&from_dropdown);
    // `identities_for_account` always inserts the account's own default
    // identity at index 0, so anything else selected is an alternate one.
    // Providers (Gmail in particular) silently substitute the account's own
    // address on delivery unless that alternate address is registered and
    // verified as a "send as"/alias address server-side, which is easy to
    // forget and looks like a Lookout bug when it bites - this icon is just
    // a nudge to check that, not a guarantee the address will work.
    let identity_hint_icon = gtk::Image::from_icon_name("dialog-warning-symbolic");
    identity_hint_icon.set_tooltip_text(Some(
        "Sending as an alternate address. Make sure it's registered and verified as a \"send as\"/alias address with your mail provider, or recipients may see your account's own address instead.",
    ));
    identity_hint_icon.set_visible(false);
    let update_identity_hint: Rc<dyn Fn()> = {
        let from_dropdown = from_dropdown.clone();
        let identity_hint_icon = identity_hint_icon.clone();
        Rc::new(move || {
            identity_hint_icon.set_visible(from_dropdown.selected() != 0);
        })
    };
    {
        let update_identity_hint = update_identity_hint.clone();
        from_dropdown.connect_selected_notify(move |_| update_identity_hint());
    }
    let refresh_from_dropdown: Rc<dyn Fn()> = {
        let identities = identities.clone();
        let from_dropdown = from_dropdown.clone();
        let update_identity_hint = update_identity_hint.clone();
        Rc::new(move || {
            let list = identities_source();
            let labels: Vec<String> = list.iter().map(|i| i.label()).collect();
            let label_refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let model = gtk::StringList::new(&label_refs);
            from_dropdown.set_model(Some(&model));
            *identities.borrow_mut() = list;
            if !labels.is_empty() && from_dropdown.selected() as usize >= labels.len() {
                from_dropdown.set_selected(0);
            }
            update_identity_hint();
        })
    };
    refresh_from_dropdown();
    // A reopened draft remembers which identity it was written as: select
    // that address when it's still among the account's identities. An
    // unmatched address (the identity was deleted since) silently keeps the
    // default - the saved From header's display name isn't carried over.
    if let Some(email) = prefill.from.as_deref().map(str::trim).filter(|email| !email.is_empty()) {
        let index = identities.borrow().iter().position(|identity| identity.email.eq_ignore_ascii_case(email));
        if let Some(index) = index {
            from_dropdown.set_selected(index as u32);
        }
    }

    let fields_group = adw::PreferencesGroup::new();
    fields_group.add(to_row.widget());
    fields_group.add(cc_row.widget());
    fields_group.add(bcc_row.widget());
    fields_group.add(&subject_row);

    // --- Attachments: a canonical in-session list, rendered as removable
    // chips below the fields, same repopulate-from-truth shape
    // `RecipientEntry` uses for To/Cc/Bcc. A reopened draft seeds the list
    // with the saved message's decoded parts (item 1.12); everything else
    // starts empty. ---
    let attachments: Rc<RefCell<Vec<PendingAttachment>>> = Rc::new(RefCell::new(Vec::new()));
    let attachment_flow = gtk::FlowBox::builder()
        .selection_mode(gtk::SelectionMode::None)
        .row_spacing(4)
        .column_spacing(4)
        .min_children_per_line(1)
        .max_children_per_line(64)
        .homogeneous(false)
        .visible(false)
        .build();
    for attachment in &prefill.attachments {
        attachments.borrow_mut().push(PendingAttachment {
            filename: attachment.filename.clone(),
            content_type: attachment.content_type.clone(),
            bytes: Rc::new(attachment.bytes.clone()),
        });
    }
    rebuild_attachment_chips(&attachments, &attachment_flow);

    // --- Read-receipt request: RFC 8098's `Disposition-Notification-To`
    // header, asking recipients to notify us when they read the message.
    // The header is only added at build time (see `build_raw_message`), so
    // the toggle's state flows through the draft snapshot and the send. It
    // lives in the header next to the pop-out button rather than as its own
    // field row (built and placed into `top_row` further down).
    let read_receipt_toggle = gtk::ToggleButton::builder()
        .icon_name("mark-location-symbolic")
        .tooltip_text("Request read receipt")
        .build();

    // --- Importance: Outlook's High/Low buttons (RFC 2156 `Importance`,
    // `X-Priority`, `X-MSMail-Priority` - written by `build_raw_message`).
    // Two independent toggles kept mutually exclusive; neither active is
    // Normal. Both live in the editor toolbar, which the window layer moves
    // as a whole between the reading pane and a pop-out window, so they need
    // no special reparenting of their own.
    let high_importance_toggle = gtk::ToggleButton::builder().icon_name("emblem-important-symbolic").tooltip_text("High importance").build();
    let low_importance_toggle = gtk::ToggleButton::builder().icon_name("go-down-symbolic").tooltip_text("Low importance").build();
    {
        let other = low_importance_toggle.clone();
        high_importance_toggle.connect_toggled(move |toggle| {
            if toggle.is_active() {
                other.set_active(false);
            }
        });
    }
    {
        let other = high_importance_toggle.clone();
        low_importance_toggle.connect_toggled(move |toggle| {
            if toggle.is_active() {
                other.set_active(false);
            }
        });
    }
    // A reopened draft restores its saved Importance and read-receipt
    // request; every other prefill leaves both at Normal/off. The toggles'
    // handlers are already connected, so High/Low stay mutually exclusive.
    match prefill.importance {
        Importance::High => high_importance_toggle.set_active(true),
        Importance::Low => low_importance_toggle.set_active(true),
        Importance::Normal => {}
    }
    read_receipt_toggle.set_active(prefill.request_read_receipt);

    // --- Sensitivity: RFC 2156 `Sensitivity` (Normal / Personal / Private /
    // Company-Confidential), written by `build_raw_message` when it isn't
    // Normal. A dropdown trailing the importance buttons in the editor
    // toolbar, which the window layer moves as a whole into a pop-out window,
    // so it needs no reparenting of its own.
    let sensitivity_model = gtk::StringList::new(&SENSITIVITY_LABELS);
    let sensitivity_dropdown = gtk::DropDown::builder().model(&sensitivity_model).tooltip_text("Sensitivity").build();
    sensitivity_dropdown.set_selected(sensitivity_index(prefill.sensitivity));

    let body_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    if let Some(body) = &prefill.body {
        let buffer = body_view.buffer();
        buffer.set_text(body);
        // `set_text` leaves the "insert" mark wherever GtkTextBuffer's mark
        // gravity happens to land after a full delete+insert - not reliably
        // the top. Reply/forward bodies lead with blank lines so the user
        // can type above the quote, so the caret needs to start there too.
        buffer.place_cursor(&buffer.start_iter());
    }
    let text_scroller = gtk::ScrolledWindow::builder().child(&body_view).hexpand(true).vexpand(true).build();

    apply_spell_check(spell_check, &spell_languages);
    // A reopened HTML draft seeds the rich editor verbatim (its `cid:`
    // images already rewritten to `data:` URIs, which `read_content` turns
    // back into inline images on the next save); every other prefill - and a
    // plain-text draft - derives the markup from the plain body, since the
    // rich content must not round-trip through `text_to_html`, which would
    // flatten its formatting.
    let rich_initial = prefill.html_body.clone().unwrap_or_else(|| prefill.body.as_deref().map(text_to_html).unwrap_or_default());
    let (rich_web_view, format_toolbar) = build_rich_editor(rich_initial);

    let body_stack = gtk::Stack::builder().hexpand(true).vexpand(true).build();
    body_stack.add_named(&text_scroller, Some("text"));
    let rich_scroller = gtk::ScrolledWindow::builder().child(&*rich_web_view).hexpand(true).vexpand(true).build();
    body_stack.add_named(&rich_scroller, Some("rich"));
    body_stack.set_visible_child_name("text");

    // --- The account's default signature for this compose kind is appended
    // to the end of the body when the composer opens. Both editors are
    // seeded (the user composes in whichever one they switch to): the text
    // editor gets the plain-text form right away, and the rich editor gets
    // its HTML once the contenteditable document has finished loading (the
    // same `insertHTML`-at-end script path the paste handler and the
    // signature menu use - the rich content must not round-trip through
    // `text_to_html`, which would flatten its formatting).
    //
    // Reply/forward prefills lead with blank lines so the user can type
    // above the quoted/forwarded content - `text_to_html` now renders those
    // as leading empty paragraphs, and once the document has loaded the
    // caret needs to be parked there too (WebKit doesn't otherwise put it
    // anywhere in particular). That has to happen *after* any signature
    // append below, not race it.
    let has_prefill_body = prefill.body.as_deref().is_some_and(|b| !b.trim().is_empty());
    let caret_to_top_script = r#"(function () {
                var range = document.createRange();
                range.selectNodeContents(document.body);
                range.collapse(true);
                var sel = window.getSelection();
                sel.removeAllRanges();
                sel.addRange(range);
            })()"#;
    if let Some(signature) = default_signature {
        let signature_text = signature.text.trim();
        if !signature_text.is_empty() {
            let buffer = body_view.buffer();
            let has_body = !buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).is_empty();
            let mut end = buffer.end_iter();
            let prefix = if has_body { "\n" } else { "" };
            buffer.insert(&mut end, &format!("{prefix}{signature_text}\n"));
        }
        let signature_html = signature.html;
        let html_json = serde_json::to_string(&signature_html).unwrap_or_else(|_| "\"\"".to_string());
        let caret_suffix = if has_prefill_body { format!(";\n{caret_to_top_script}") } else { String::new() };
        let script = format!(
            r#"(function () {{
                var range = document.createRange();
                range.selectNodeContents(document.body);
                range.collapse(false);
                var sel = window.getSelection();
                sel.removeAllRanges();
                sel.addRange(range);
                document.execCommand('insertHTML', false, {html_json});
            }})(){caret_suffix}"#
        );
        let appended = Rc::new(Cell::new(false));
        let appended_for_handler = appended.clone();
        rich_web_view.connect_load_changed(move |view, event| {
            // `load_html` is async; the append must wait for the document.
            // The guard keeps a stray second load from appending twice.
            if event != webkit::LoadEvent::Finished || appended_for_handler.get() {
                return;
            }
            appended_for_handler.set(true);
            let view = view.clone();
            let script = script.clone();
            glib::spawn_future_local(async move {
                let _ = view.evaluate_javascript_future(&script, None, None).await;
            });
        });
    } else if has_prefill_body {
        let script = caret_to_top_script.to_string();
        let placed = Rc::new(Cell::new(false));
        let placed_for_handler = placed.clone();
        rich_web_view.connect_load_changed(move |view, event| {
            if event != webkit::LoadEvent::Finished || placed_for_handler.get() {
                return;
            }
            placed_for_handler.set(true);
            let view = view.clone();
            let script = script.clone();
            glib::spawn_future_local(async move {
                let _ = view.evaluate_javascript_future(&script, None, None).await;
            });
        });
    }

    // The plain/rich switch lives in a shared toolbar row above the body,
    // ahead of the formatting buttons - it must be reachable in both modes,
    // while the formatting buttons only make sense in rich mode and so stay
    // disabled (insensitive) otherwise.
    let rich_toggle = gtk::ToggleButton::builder()
        .label("Rich text")
        .tooltip_text("Toggle between plain text and formatted editing")
        .build();
    // Always sensitive - unlike the formatting buttons, plain-text mode can
    // carry real file attachments too.
    let attach_button = gtk::Button::from_icon_name("mail-attachment-symbolic");
    attach_button.set_tooltip_text(Some("Attach files"));
    {
        let attachments = attachments.clone();
        let attachment_flow = attachment_flow.clone();
        attach_button.connect_clicked(move |button| {
            let Some(window) = button.root().and_downcast::<gtk::Window>() else { return };
            let attachments = attachments.clone();
            let attachment_flow = attachment_flow.clone();
            glib::spawn_future_local(async move {
                let dialog = gtk::FileDialog::builder().title("Attach files").build();
                let Ok(files) = dialog.open_multiple_future(Some(&window)).await else { return };
                for i in 0..files.n_items() {
                    let Some(file) = files.item(i).and_downcast::<gio::File>() else { continue };
                    add_attachment_from_file(&file, &attachments).await;
                }
                rebuild_attachment_chips(&attachments, &attachment_flow);
            });
        });
    }
    // --- Signature menu: a "Signature" button that drops down the global
    // signature list (Config → Accounts → Signatures), one row per
    // signature inserting it at the cursor - the HTML form in rich mode,
    // the plain text in text mode - with a "Manage Signatures…" row at the
    // bottom jumping to the settings screen. Rebuilt every time the popover
    // opens, so a signature added via Settings while a composer is open
    // lands in the list immediately; the button is disabled entirely while
    // no signatures exist. The button lives at the end of the toolbar (the
    // formatting controls stay ahead of it) and carries the bundled
    // signature icon instead of a text label, matching the toolbar's other
    // icon buttons.
    let signature_image = crate::window::svg_image(
        "/io/github/gavindi/Lookout/icons/signature-1.svg",
        include_bytes!("../../../data/resources/icons/signature-1.svg"),
        18,
    );
    let signature_button = gtk::MenuButton::builder()
        .child(&signature_image)
        .css_classes(["flat"])
        .tooltip_text("Insert a signature")
        .build();
    // A visible drop-down arrow next to the icon, like other menu buttons.
    signature_button.set_always_show_arrow(true);
    let signature_popover = gtk::Popover::new();
    let signature_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    let signature_scroller = gtk::ScrolledWindow::builder()
        .child(&signature_list)
        // No fixed size: the dropdown grows to fit the whole signature
        // list, and the map handler below re-syncs its width to the field
        // rows each time it opens. The floor only guards the pre-first-map
        // state.
        .min_content_width(240)
        .build();
    // The menu's root: the scrollable signature list above, and "Manage
    // Signatures…" pinned below the list - outside the scroller - so the
    // settings shortcut is always on screen no matter how long the list
    // grows (only the list portion ever scrolls, and only when the screen
    // forces it).
    let signature_menu_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    signature_menu_box.append(&signature_scroller);
    signature_menu_box.append(&gtk::Separator::builder().orientation(gtk::Orientation::Horizontal).build());
    let manage_label = gtk::Label::builder()
        .label("Manage Signatures…")
        .xalign(0.0)
        .margin_start(10)
        .margin_end(10)
        .margin_top(3)
        .margin_bottom(3)
        .build();
    crate::privacy::mark_ui(&manage_label);
    let manage_button = gtk::Button::builder().css_classes(["flat"]).build();
    manage_button.set_child(Some(&manage_label));
    {
        let manage_signatures = manage_signatures.clone();
        let signature_popover = signature_popover.clone();
        manage_button.connect_clicked(move |_| {
            signature_popover.popdown();
            if let Some(manage) = &manage_signatures {
                manage();
            }
        });
    }
    signature_menu_box.append(&manage_button);
    signature_popover.set_child(Some(&signature_menu_box));
    signature_button.set_popover(Some(&signature_popover));

    let rebuild_signature_menu: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    {
        let signatures_source = signatures_source.clone();
        let signature_list = signature_list.clone();
        let signature_button = signature_button.clone();
        *rebuild_signature_menu.borrow_mut() = Box::new(move || {
            while let Some(child) = signature_list.first_child() {
                signature_list.remove(&child);
            }
            let signatures = signatures_source();
            signature_button.set_sensitive(!signatures.is_empty());
            // Plain labels are appended (the ListBox wraps each in a row),
            // matching the recipient-suggestions popover. "Manage
            // Signatures…" is not part of the list - it's pinned below the
            // scroller.
            for signature in &signatures {
                let label = gtk::Label::builder()
                    .label(&signature.name)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .margin_start(10)
                    .margin_end(10)
                    .margin_top(6)
                    .margin_bottom(6)
                    .build();
                signature_list.append(&label);
            }
        });
    }
    (rebuild_signature_menu.borrow())();
    {
        let rebuild_handle = rebuild_signature_menu.clone();
        let signature_scroller = signature_scroller.clone();
        let fields_group = fields_group.clone();
        signature_popover.connect_map(move |_| {
            // Match the dropdown's width to half the composer's field rows
            // (the To/Cc/Bcc/Subject entries): the fields group spans
            // exactly the entries' width. Runs while the popover is
            // mapping, before its size is measured, so the new width lands
            // on the menu that's about to open.
            let width = (fields_group.width() / 2).max(240);
            signature_scroller.set_min_content_width(width);
            (rebuild_handle.borrow())();
        });
    }
    // One activation handler for the whole list, like the recipient
    // suggestions: the index is resolved against the *current* signature
    // list at click time, so a rebuild can never leave a stale handler
    // mapping a row onto the wrong signature.
    {
        let signatures_source = signatures_source.clone();
        let signature_popover = signature_popover.clone();
        let rich_web_view = rich_web_view.clone();
        let body_view = body_view.clone();
        let rich_toggle = rich_toggle.clone();
        signature_list.connect_row_activated(move |_, row| {
            let index = row.index() as usize;
            signature_popover.popdown();
            let signatures = signatures_source();
            let Some(signature) = signatures.get(index) else { return };
            if rich_toggle.is_active() {
                // Rich mode: insert the signature's HTML at the editor's
                // cursor. `webkit_web_view_execute_editing_command_with_argument`
                // doesn't reliably resolve WebKit's "InsertHTML" editing
                // command (the paste handler's `document.execCommand` JS
                // path is the proven one - same round trip, minus the C
                // command-name translation), so the insertion goes through
                // `evaluate_javascript` like the paste path. The script
                // restores the editor's focus (the popover click blurred
                // it), keeps the user's caret/selection when one exists,
                // and falls back to appending at the end of the body when
                // the selection was cleared - an insertion must never
                // silently vanish.
                let html_json = serde_json::to_string(&signature.html).unwrap_or_else(|_| "\"\"".to_string());
                let script = format!(
                    r#"(function () {{
                        document.body.focus();
                        var sel = window.getSelection();
                        if (!sel || sel.rangeCount === 0) {{
                            var range = document.createRange();
                            range.selectNodeContents(document.body);
                            range.collapse(false);
                            sel.removeAllRanges();
                            sel.addRange(range);
                        }}
                        document.execCommand('insertHTML', false, {html_json});
                    }})()"#
                );
                let web_view = rich_web_view.clone();
                glib::spawn_future_local(async move {
                    let _ = web_view.evaluate_javascript_future(&script, None, None).await;
                });
            } else {
                body_view.buffer().insert_at_cursor(&signature.text);
            }
        });
    }
    // --- My Templates menu (item 2.10 / C-17): like the signature menu, a
    // button dropping down the global template list, one row per template
    // inserting its snippet at the cursor - the HTML form in rich mode, the
    // plain text in text mode - with a "Manage Templates…" row pinned at the
    // bottom. Rebuilt every time the popover opens; disabled while no templates
    // exist.
    let templates_image = gtk::Image::from_icon_name("edit-copy-symbolic");
    let templates_button = gtk::MenuButton::builder()
        .child(&templates_image)
        .css_classes(["flat"])
        .tooltip_text("Insert a template")
        .build();
    templates_button.set_always_show_arrow(true);
    let templates_popover = gtk::Popover::new();
    let templates_list = gtk::ListBox::builder().selection_mode(gtk::SelectionMode::Single).build();
    let templates_scroller = gtk::ScrolledWindow::builder().child(&templates_list).min_content_width(240).build();
    let templates_menu_box = gtk::Box::builder().orientation(gtk::Orientation::Vertical).build();
    templates_menu_box.append(&templates_scroller);
    templates_menu_box.append(&gtk::Separator::builder().orientation(gtk::Orientation::Horizontal).build());
    let manage_templates_label = gtk::Label::builder()
        .label("Manage Templates…")
        .xalign(0.0)
        .margin_start(10)
        .margin_end(10)
        .margin_top(3)
        .margin_bottom(3)
        .build();
    crate::privacy::mark_ui(&manage_templates_label);
    let manage_templates_button = gtk::Button::builder().css_classes(["flat"]).build();
    manage_templates_button.set_child(Some(&manage_templates_label));
    {
        let manage_templates = manage_templates.clone();
        let templates_popover = templates_popover.clone();
        manage_templates_button.connect_clicked(move |_| {
            templates_popover.popdown();
            if let Some(manage) = &manage_templates {
                manage();
            }
        });
    }
    templates_menu_box.append(&manage_templates_button);
    templates_popover.set_child(Some(&templates_menu_box));
    templates_button.set_popover(Some(&templates_popover));

    let rebuild_templates_menu: Rc<RefCell<Box<dyn Fn()>>> = Rc::new(RefCell::new(Box::new(|| {})));
    {
        let templates_source = templates_source.clone();
        let templates_list = templates_list.clone();
        let templates_button = templates_button.clone();
        *rebuild_templates_menu.borrow_mut() = Box::new(move || {
            while let Some(child) = templates_list.first_child() {
                templates_list.remove(&child);
            }
            let templates = templates_source();
            templates_button.set_sensitive(!templates.is_empty());
            for template in &templates {
                let label = gtk::Label::builder()
                    .label(&template.name)
                    .xalign(0.0)
                    .ellipsize(gtk::pango::EllipsizeMode::End)
                    .margin_start(10)
                    .margin_end(10)
                    .margin_top(6)
                    .margin_bottom(6)
                    .build();
                templates_list.append(&label);
            }
        });
    }
    (rebuild_templates_menu.borrow())();
    {
        let rebuild_handle = rebuild_templates_menu.clone();
        let templates_scroller = templates_scroller.clone();
        let fields_group = fields_group.clone();
        templates_popover.connect_map(move |_| {
            let width = (fields_group.width() / 2).max(240);
            templates_scroller.set_min_content_width(width);
            (rebuild_handle.borrow())();
        });
    }
    // One activation handler for the whole list, like the signature menu: the
    // index is resolved against the *current* template list at click time.
    {
        let templates_source = templates_source.clone();
        let templates_popover = templates_popover.clone();
        let rich_web_view = rich_web_view.clone();
        let body_view = body_view.clone();
        let rich_toggle = rich_toggle.clone();
        templates_list.connect_row_activated(move |_, row| {
            let index = row.index() as usize;
            templates_popover.popdown();
            let templates = templates_source();
            let Some(template) = templates.get(index) else { return };
            if rich_toggle.is_active() {
                // Same `document.execCommand('insertHTML')` path as the
                // signature menu, which restores focus and keeps the caret.
                let html_json = serde_json::to_string(&template.html).unwrap_or_else(|_| "\"\"".to_string());
                let script = format!(
                    r#"(function () {{
                        document.body.focus();
                        var sel = window.getSelection();
                        if (!sel || sel.rangeCount === 0) {{
                            var range = document.createRange();
                            range.selectNodeContents(document.body);
                            range.collapse(false);
                            sel.removeAllRanges();
                            sel.addRange(range);
                        }}
                        document.execCommand('insertHTML', false, {html_json});
                    }})()"#
                );
                let web_view = rich_web_view.clone();
                glib::spawn_future_local(async move {
                    let _ = web_view.evaluate_javascript_future(&script, None, None).await;
                });
            } else {
                body_view.buffer().insert_at_cursor(&template.text);
            }
        });
    }
    // --- The composer's command ribbon. The controls were built above; the
    // ribbon dresses each with an icon and a label and groups them like
    // Outlook's Message tab. The dense inline-formatting row
    // (`format_toolbar`) keeps its glyphs, as Outlook's does, and rides in
    // the "Format" group as a unit.
    let editor_band = crate::ribbon::RibbonBand::new(&ribbon_style, &["toolbar"]);
    editor_band.group("Actions", |group| {
        group.button(&attach_button, "mail-attachment-symbolic", "Attach files");
        group.toggle(
            &rich_toggle,
            crate::window::themed_icon_name(&["document-edit-symbolic", "accessories-text-editor-symbolic"]),
            "Rich text",
        );
    });
    editor_band.group("Insert", |group| {
        group.menu(&signature_button, "signature-1", "Signature");
        group.menu(&templates_button, "edit-copy-symbolic", "Templates");
    });
    editor_band.group("Format", |group| {
        group.widget(&format_toolbar);
    });
    editor_band.group("Options", |group| {
        group.toggle(&high_importance_toggle, "emblem-important-symbolic", "High");
        group.toggle(&low_importance_toggle, "go-down-symbolic", "Low");
        group.widget(&sensitivity_dropdown);
    });
    let editor_toolbar = editor_band.root().clone();

    {
        let web_view = rich_web_view.clone();
        let stack_for_toggle = body_stack.clone();
        let format_toolbar = format_toolbar.clone();
        rich_toggle.connect_toggled(move |toggle| {
            format_toolbar.set_sensitive(toggle.is_active());
            if toggle.is_active() {
                stack_for_toggle.set_visible_child_name("rich");
                web_view.grab_focus();
            } else {
                stack_for_toggle.set_visible_child_name("text");
            }
        });
    }
    // Wire the Config → Mail preference into the composer's opening mode. The
    // toggled handler above is connected first, so `set_active` also routes
    // the stack to the right page, focuses the editor, and arms the toolbar.
    format_toolbar.set_sensitive(rich_text_default);
    rich_toggle.set_active(rich_text_default);

    // --- Draft autosave state ---
    // Set when the composer closes (Send, Cancel, or being unrooted) so the
    // autosave timer can't fire against a torn-down page.
    let closed = Rc::new(Cell::new(false));
    let status_label = gtk::Label::builder().css_classes(["dim-label"]).build();
    crate::privacy::mark_ui(&status_label);
    // A reopened draft keeps the saved message's own Message-ID (bare, no
    // `<>` - `build_raw_message` re-adds the brackets), so resaves replace
    // the stored copy; a fresh composer mints a new stable id.
    let draft_message_id = Rc::new(prefill.draft_message_id.clone().unwrap_or_else(new_message_id));
    // The exact source copy for the first save of a reopened draft; taken -
    // and cleared - by `save_if_changed`'s first successful queue.
    let replace_uid: Rc<RefCell<Option<(MailboxId, Uid)>>> = Rc::new(RefCell::new(prefill.draft_source.clone()));

    let autosave = AutosaveCtx {
        to_row: to_row.clone(),
        cc_row: cc_row.clone(),
        bcc_row: bcc_row.clone(),
        subject_row: subject_row.clone(),
        read_receipt_toggle: read_receipt_toggle.clone(),
        high_importance_toggle: high_importance_toggle.clone(),
        low_importance_toggle: low_importance_toggle.clone(),
        sensitivity_dropdown: sensitivity_dropdown.clone(),
        rich_toggle: rich_toggle.clone(),
        body_view: body_view.clone(),
        rich_web_view: rich_web_view.clone(),
        attachments: attachments.clone(),
        last_saved: Rc::new(RefCell::new(None)),
        // A reopened draft is already stored: its first save must replace,
        // not append (and its Send must delete the stored copy).
        draft_queued: Rc::new(Cell::new(prefill.draft_source.is_some() || prefill.draft_saved)),
        in_flight: Rc::new(Cell::new(false)),
        draft_message_id: draft_message_id.clone(),
        replace_uid,
        cmd_tx: cmd_tx.clone(),
        identities: identities.clone(),
        from_dropdown: from_dropdown.clone(),
        in_reply_to: prefill.in_reply_to.clone(),
        references: prefill.references.clone(),
        calendar_part: prefill.calendar_part.clone(),
        status_label: status_label.clone(),
    };

    // The autosave tick: fixed interval, change-detected by snapshot
    // comparison rather than edit signals (WebKit's contenteditable view has
    // no usable "content changed" signal, so polling is the only mode that
    // covers both body editors uniformly).
    {
        let autosave = autosave.clone();
        let closed = closed.clone();
        glib::spawn_future_local(async move {
            loop {
                glib::timeout_future(DRAFT_AUTOSAVE_INTERVAL).await;
                if closed.get() {
                    break;
                }
                autosave.attempt().await;
            }
        });
    }

    // DraftSaved confirmations arrive on the account's event loop and are
    // relayed here; flip the status label once the save is actually
    // server-side. Events for other compose sessions' ids (stale relay
    // delivery) are ignored.
    let (draft_saved_tx, draft_saved_rx) = async_channel::bounded(8);
    {
        let draft_message_id = draft_message_id.clone();
        let status_label = status_label.clone();
        glib::spawn_future_local(async move {
            while let Ok(message_id) = draft_saved_rx.recv().await {
                if message_id == *draft_message_id {
                    status_label.set_label("Draft saved");
                }
            }
        });
    }

    let cancel_button = gtk::Button::from_icon_name(crate::window::themed_icon_name(&["user-trash-symbolic", "edit-delete-symbolic"]));
    cancel_button.add_css_class("destructive-action");
    cancel_button.set_tooltip_text(Some("Cancel"));
    // Send is a split button, as in Outlook: the main part sends now, the
    // arrow opens the "Schedule send" menu (item 1.14), wired below.
    let send_button = adw::SplitButton::builder()
        .label("Send")
        .css_classes(["suggested-action"])
        .dropdown_tooltip("Schedule send")
        .build();
    // Not shown while embedded - it's redundant next to the compose fields
    // right below it - but kept in `top_row` (invisible) as the anchor
    // `reorder_child_after` puts the status label back next to on
    // pop-back-in; the pop-out window shows the title in its own title bar
    // instead (see `PopOutHandle`'s docs).
    let title_label = gtk::Label::builder().label(title).visible(false).build();
    let header_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    header_spacer.set_hexpand(true);

    // The header's pop-out button: moves this (still fully alive) composer
    // into its own window. Only built when the caller arms it; hidden while
    // the composer lives in the pop-out window and re-shown when it pops
    // back into the reading pane (see the `root-notify` handler below).
    let moving = Rc::new(Cell::new(false));
    let popped_out = Rc::new(Cell::new(false));
    let pop_out_button = gtk::Button::from_icon_name(crate::window::themed_icon_name(&["popout1", "window-new-symbolic", "view-restore-symbolic"]));
    pop_out_button.set_tooltip_text(Some("Pop out compose window"));

    let top_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    top_row.append(&send_button);
    top_row.append(&from_dropdown);
    top_row.append(&identity_hint_icon);
    top_row.append(&header_spacer);
    top_row.append(&title_label);
    top_row.append(&status_label);
    top_row.append(&read_receipt_toggle);
    if on_pop_out.is_some() {
        top_row.append(&pop_out_button);
    }
    top_row.append(&cancel_button);

    let content = gtk::Box::builder()
        .orientation(gtk::Orientation::Vertical)
        .spacing(12)
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();
    content.append(&top_row);
    content.append(&fields_group);
    content.append(&attachment_flow);
    content.append(&editor_toolbar);
    content.append(&body_stack);

    // File drop (item 1.20 / C-15): dropping files onto the composer body
    // attaches them - on the plain-text and rich editors and the column
    // around them, so a drop anywhere in the message area is caught.
    install_file_drop_target(&content, attachments.clone(), attachment_flow.clone());
    install_file_drop_target(&body_view, attachments.clone(), attachment_flow.clone());
    install_file_drop_target(&*rich_web_view, attachments.clone(), attachment_flow.clone());

    {
        let on_done = on_done.clone();
        let autosave = autosave.clone();
        let closed = closed.clone();
        cancel_button.connect_clicked(move |_| {
            // One final save before teardown, so closing the composer never
            // loses work (the draft stays in Drafts; there's no "discard"
            // affordance yet). The save runs on its own task and outlives
            // the widget - `AutosaveCtx` holds the editor by `Rc`, so the
            // read still resolves after the pane has closed, and the task
            // never consults `closed` (which is set right below, and would
            // otherwise veto the very save this exists to perform).
            let autosave = autosave.clone();
            glib::spawn_future_local(async move { autosave.attempt().await });
            closed.set(true);
            on_done();
        });
    }
    {
        let in_reply_to = prefill.in_reply_to;
        let references = prefill.references;
        let calendar_part = prefill.calendar_part.clone();
        let prefill_body_text = prefill.body.clone().unwrap_or_default();
        let autosave = autosave.clone();
        let draft_message_id = draft_message_id.clone();
        let closed = closed.clone();
        let from_dropdown = from_dropdown.clone();
        let read_receipt_toggle = read_receipt_toggle.clone();
        let high_importance_toggle = high_importance_toggle.clone();
        let low_importance_toggle = low_importance_toggle.clone();
        let on_send_started = on_send_started.clone();
        // The Send button anchors the attachment-reminder dialog (item 1.19);
        // cloned before the closure captures it by move.
        let send_button_for_reminder = send_button.clone();
        // Shared by Send (`None`: now) and the schedule-send menu (`Some`:
        // the picked time, item 1.14).
        let send_composed: Rc<dyn Fn(Option<chrono::DateTime<chrono::Utc>>)> = Rc::new(move |schedule| {
            // Commit anything typed but not yet turned into a chip, so the
            // field ends up showing exactly what is about to be sent.
            for row in [&to_row, &cc_row, &bcc_row] {
                row.commit_pending();
            }
            let to = to_row.addresses();
            if to.is_empty() {
                return;
            }
            let cc = cc_row.addresses();
            let bcc = bcc_row.addresses();
            // Everything from here on has to await the editor read, so the
            // rest of the send runs on its own task. `closed` and `on_done`
            // stay behind at the end of it rather than firing early: the
            // pane must not close before the body has been read out of the
            // (still-parented) WebView.
            let rich = rich_toggle.is_active();
            let body_view = body_view.clone();
            let rich_web_view = rich_web_view.clone();
            let subject = subject_row.text().to_string();
            let prefill_body_text = prefill_body_text.clone();
            let autosave = autosave.clone();
            let draft_message_id = draft_message_id.clone();
            let cmd_tx = cmd_tx.clone();
            let identities = identities.clone();
            let from_dropdown = from_dropdown.clone();
            let read_receipt_toggle = read_receipt_toggle.clone();
            let high_importance_toggle = high_importance_toggle.clone();
            let low_importance_toggle = low_importance_toggle.clone();
            let sensitivity_dropdown = sensitivity_dropdown.clone();
            let attachments = attachments.clone();
            let in_reply_to = in_reply_to.clone();
            let references = references.clone();
            let calendar_part = calendar_part.clone();
            let closed = closed.clone();
            let on_done = on_done.clone();
            let on_send_started = on_send_started.clone();
            let on_schedule_send = on_schedule_send.clone();
            // The Send button anchors the attachment-reminder dialog (item
            // 1.19); cloned because the async block captures it by move.
            let send_button_for_dialog = send_button_for_reminder.clone();
            glib::spawn_future_local(async move {
                // A scheduled message waits in Drafts until it goes out, as
                // in new Outlook (UX-45): save it now so the Drafts copy is
                // current and `draft_queued` names it for the outbox to
                // delete once sent. Commands reach the session in order, so
                // the save lands before the outbox row is queued.
                if schedule.is_some() {
                    autosave.flush().await;
                }
                let (text_body, html_body, inline_images) = if rich {
                    // Reading the editor's live HTML back out is a round trip
                    // through WebKit; if that fails (e.g. the page hasn't
                    // finished loading yet) fall back to the prefill body so a
                    // Send click can never silently drop the message.
                    match read_content(&rich_web_view).await {
                        Some(content) => {
                            let text = content.text.trim().to_string();
                            // A body that's purely an inserted image (no
                            // surrounding text) has no visible text at all -
                            // `innerText` only sees text nodes - so emptiness
                            // alone can't mean "nothing worth sending" here;
                            // an image with no caption is still real content.
                            if text.is_empty() && content.images.is_empty() {
                                (String::new(), None, vec![])
                            } else {
                                let html = content.html.trim().to_string();
                                (text, Some(html), content.images)
                            }
                        }
                        None => (prefill_body_text, None, vec![]),
                    }
                } else {
                    let buffer = body_view.buffer();
                    let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
                    (text, None, vec![])
                };
                // Attachment reminder (item 1.19 / C-16): if the message
                // mentions an attachment but has none, ask before sending. A
                // "Don't Send" answer returns here, leaving the composer open
                // so the user can attach the file; nothing is queued (and
                // `closed`/`on_done` stay unset).
                if warn_no_attachment && attachments.borrow().is_empty() && looks_like_missing_attachment(&subject, &text_body) {
                    let (respond_tx, respond_rx) = async_channel::bounded(1);
                    let dialog = adw::AlertDialog::builder()
                        .heading("Send this message without an attachment?")
                        .body("Your message mentions an attachment, but none is attached.")
                        .default_response("cancel")
                        .close_response("cancel")
                        .build();
                    dialog.add_response("cancel", "Don't Send");
                    dialog.add_response("send", "Send Anyway");
                    dialog.set_response_appearance("cancel", adw::ResponseAppearance::Suggested);
                    dialog.connect_response(None, move |_dialog, response| {
                        let _ = respond_tx.send_blocking(response == "send");
                    });
                    let anchor = send_button_for_dialog.root().and_downcast::<gtk::Window>();
                    dialog.present(anchor.as_ref());
                    if !matches!(respond_rx.recv().await, Ok(true)) {
                        return;
                    }
                }
                // If this message was draft-autosaved, reuse the draft's
                // stable `Message-ID` on the sent copy and let the account
                // session's outbox delete the Drafts copy *after* the message
                // actually goes out (item 0.3) - so a queued send that fails or
                // is scheduled leaves the draft in Drafts until it's sent.
                let draft_message_id_used = autosave.draft_queued.get().then(|| (*draft_message_id).clone());
                let identity = selected_identity(&identities, &from_dropdown);
                let bcc = with_identity_bcc(bcc.clone(), &identity);
                let msg = ComposedMessage {
                    from: identity.email,
                    display_name: (!identity.name.trim().is_empty()).then(|| identity.name.clone()),
                    to,
                    cc,
                    bcc,
                    reply_to: identity.reply_to.iter().map(|a| a.address.clone()).collect(),
                    subject,
                    text_body,
                    html_body,
                    attachments: attachments.borrow().iter().map(PendingAttachment::to_mail_attachment).collect(),
                    inline_images,
                    calendar_part,
                    read_receipt: None,
                    request_read_receipt: read_receipt_toggle.is_active(),
                    importance: selected_importance(&high_importance_toggle, &low_importance_toggle),
                    sensitivity: selected_sensitivity(&sensitivity_dropdown),
                    in_reply_to,
                    references,
                    message_id: draft_message_id_used,
                };
                // On a Microsoft 365 account, schedule server-side through
                // Exchange (item 5.5 / C-08) so the message goes out even with
                // Lookout closed; otherwise fall through to the local outbox.
                // An iMIP reply or read receipt is never guest-scheduled (its
                // special MIME body has no Graph equivalent here).
                if let Some(at) = schedule {
                    if msg.calendar_part.is_none() && msg.read_receipt.is_none() && on_schedule_send.as_ref().is_some_and(|schedule_send| schedule_send(&msg, at)) {
                        closed.set(true);
                        on_done();
                        return;
                    }
                }
                let (not_before, client_token) = on_send_started(&msg, schedule);
                let _ = cmd_tx.send_blocking(AccountCommand::SendMessage {
                    message: Box::new(msg),
                    not_before,
                    client_token,
                });
                closed.set(true);
                on_done();
            });
        });
        {
            let send_composed = send_composed.clone();
            send_button.connect_clicked(move |_| send_composed(None));
        }
        send_button.set_popover(Some(&build_schedule_popover(Rc::new(move |at| send_composed(Some(at))))));
    }

    // --- Pop-out button wiring: hand the composer (still alive) to the
    // caller so it can move it into its own window. The flags must be set
    // *before* the move and cleared after it returns: the removal unroots
    // the composer, and the `root-notify` handler below would otherwise read
    // that as a displacement and kill the autosave. All of this runs
    // synchronously on the main loop, so the autosave tick can never observe
    // the transient unroot.
    if let Some(on_pop_out) = on_pop_out {
        let moving = moving.clone();
        let popped_out = popped_out.clone();
        let closed = closed.clone();
        let content = content.clone();
        let top_row = top_row.clone();
        let title_label = title_label.clone();
        let status_label = status_label.clone();
        let cancel_button = cancel_button.clone();
        let send_button = send_button.clone();
        let from_dropdown = from_dropdown.clone();
        let read_receipt_toggle = read_receipt_toggle.clone();
        let editor_toolbar = editor_toolbar.clone();
        let attachment_flow = attachment_flow.clone();
        pop_out_button.connect_clicked(move |_| {
            if popped_out.get() {
                return;
            }
            moving.set(true);
            popped_out.set(true);
            on_pop_out(PopOutHandle {
                widget: content.clone(),
                editor_toolbar: editor_toolbar.clone(),
                toolbar_anchor: attachment_flow.clone().into(),
                cancel_button: cancel_button.clone(),
                send_button: send_button.clone(),
                from_dropdown: from_dropdown.clone(),
                read_receipt_toggle: read_receipt_toggle.clone(),
                top_row: top_row.clone(),
                title_label: title_label.clone(),
                status_label: status_label.clone(),
                moving: moving.clone(),
                popped_out: popped_out.clone(),
                done: closed.clone(),
            });
            moving.set(false);
        });
        // "Edit response before sending" (CAL-15) opens straight into the
        // window: run the exact pop-out path once the composer has been
        // embedded, without waiting for a click on a button the user would
        // never see.
        if pop_out_on_open {
            let pop_out_button = pop_out_button.clone();
            glib::idle_add_local_once(move || pop_out_button.emit_clicked());
        }
    }

    // Belt and braces for the autosave timer: Cancel and Send set `closed`
    // themselves, but a composer displaced some other way (a second composer
    // opening over this one) would otherwise leave its 5-second loop running
    // forever against a detached editor - still writing drafts. A pop-out is
    // not a displacement: `moving` is set around the removal, so the flag
    // gates the kill. Re-rooting (the composer landing in the pop-out window
    // or popping back into the reading pane) re-syncs the pop-out button's
    // visibility to `popped_out` - it's hidden while the composer lives in
    // its own window, where the button would be a pointless no-op.
    {
        let closed = closed.clone();
        let moving = moving.clone();
        let popped_out = popped_out.clone();
        let pop_out_button = pop_out_button.clone();
        let top_row = top_row.clone();
        content.connect_root_notify(move |widget| {
            if widget.root().is_none() {
                if !moving.get() {
                    closed.set(true);
                }
            } else {
                // While popped out the window's own title bar replaces the
                // composer's header row entirely - it shows the title,
                // hosts Cancel/Send (and the status label, relocated there
                // by the window layer), and makes the pop-out button a
                // no-op - so the row is hidden; it comes back with the
                // whole composer on pop-back-in.
                let shown = !popped_out.get();
                pop_out_button.set_visible(shown);
                top_row.set_visible(shown);
            }
        });
    }

    (content, editor_toolbar, draft_saved_tx, refresh_from_dropdown)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{TimeZone, Utc};
    use lookout_core::{AccountId, BodyPart, EmailAddress, MailboxId, ThreadKey, Uid};
    use std::collections::BTreeSet;

    fn sample_summary() -> EmailSummary {
        let account_id = AccountId("/test/account".to_string());
        EmailSummary {
            inference_classification: Default::default(),
            uid: Uid(1),
            mailbox: MailboxId::new(&account_id, "INBOX"),
            message_id: Some("orig@example.com".to_string()),
            in_reply_to: None,
            references: Vec::new(),
            thread_key: ThreadKey(String::new()),
            subject: Some("Hello there".to_string()),
            from: vec![EmailAddress {
                name: Some("Ada Lovelace".to_string()),
                address: "ada@example.com".to_string(),
            }],
            to: vec![
                EmailAddress {
                    name: None,
                    address: "me@example.com".to_string(),
                },
                EmailAddress {
                    name: None,
                    address: "other@example.com".to_string(),
                },
            ],
            cc: vec![EmailAddress {
                name: None,
                address: "cc-person@example.com".to_string(),
            }],
            date: Utc.with_ymd_and_hms(2026, 7, 30, 12, 0, 0).unwrap(),
            flags: BTreeSet::new(),
            keywords: BTreeSet::new(),
            size: 100,
            has_attachment: false,
            has_calendar: false,
            preview: None,
            mentions: false,
            importance: Default::default(),
            sensitivity: Default::default(),
            list_id: None,
            structure: None,
        }
    }

    fn sample_body(headers: Vec<(String, String)>, text_body: Option<&str>) -> EmailBody {
        EmailBody {
            uid: Uid(1),
            text_body: text_body.map(str::to_string),
            html_body: None,
            calendar_ics: None,
            parts: Vec::<BodyPart>::new(),
            headers,
            auth_results: None,
        }
    }

    #[test]
    fn header_value_matches_case_insensitively() {
        let headers = vec![("Message-ID".to_string(), "<abc@example.com>".to_string())];
        assert_eq!(header_value(&headers, "message-id"), Some("<abc@example.com>"));
        assert_eq!(header_value(&headers, "Message-Id"), Some("<abc@example.com>"));
        assert_eq!(header_value(&headers, "subject"), None);
    }

    /// Reopening a draft restores everything the saved copy carried: named
    /// recipients, both bodies, Bcc, the identity it was written as,
    /// importance/receipt/threading, attachment bytes, and the
    /// `(mailbox, uid)`/`Message-ID` the autosave replaces on save. Nothing
    /// is filtered or rewritten, unlike Reply/Forward.
    #[test]
    fn draft_prefill_restores_the_saved_copy_verbatim() {
        let mut summary = sample_summary();
        summary.message_id = Some("<draft-1@lookout.local>".to_string());
        summary.importance = Importance::High;
        let content = DraftPrefillContent {
            text_body: Some("draft text".to_string()),
            html_body: Some("<p>draft text</p>".to_string()),
            attachments: vec![Attachment {
                filename: "notes.txt".to_string(),
                content_type: "text/plain".to_string(),
                bytes: b"notes".to_vec(),
            }],
            bcc: vec!["Ada <hidden@example.com>".to_string()],
            in_reply_to: Some("older@example.com".to_string()),
            references: vec!["older@example.com".to_string()],
            request_read_receipt: true,
            sensitivity: MessageSensitivity::Private,
            ..Default::default()
        };

        let prefill = build_draft_prefill(&summary, &content);
        assert_eq!(prefill.to.as_deref(), Some("me@example.com, other@example.com"));
        assert_eq!(prefill.cc.as_deref(), Some("cc-person@example.com"));
        assert_eq!(prefill.bcc.as_deref(), Some("Ada <hidden@example.com>"));
        assert_eq!(prefill.subject.as_deref(), Some("Hello there"));
        assert_eq!(prefill.body.as_deref(), Some("draft text"));
        assert_eq!(prefill.html_body.as_deref(), Some("<p>draft text</p>"));
        assert_eq!(prefill.from.as_deref(), Some("ada@example.com"));
        assert_eq!(prefill.importance, Importance::High);
        assert_eq!(prefill.sensitivity, MessageSensitivity::Private);
        assert!(prefill.request_read_receipt);
        assert_eq!(prefill.attachments.len(), 1);
        assert_eq!(prefill.attachments[0].bytes, b"notes");
        assert_eq!(prefill.draft_message_id.as_deref(), Some("draft-1@lookout.local"));
        assert_eq!(prefill.draft_source.as_ref().map(|(m, u)| (m.clone(), *u)), Some((summary.mailbox.clone(), summary.uid)));
        assert_eq!(prefill.in_reply_to.as_deref(), Some("older@example.com"));
        assert_eq!(prefill.references, vec!["older@example.com".to_string()]);
    }

    /// A send withdrawn from the outbox with no in-memory copy (schedule
    /// send's "Cancel send"/"Edit", item 1.14) reopens from the queued raw
    /// message: envelope from its headers, Bcc from the payload (it's never a
    /// header on a sent message), bodies with the inline image as a `data:`
    /// URI, attachments, importance, receipt and threading - and the Drafts
    /// copy's id, so the composer replaces it.
    #[test]
    fn outbox_prefill_reopens_a_withdrawn_scheduled_send() {
        let msg = ComposedMessage {
            from: "ada@example.com".to_string(),
            display_name: Some("Ada".to_string()),
            to: vec!["Bob <bob@example.com>".to_string()],
            cc: vec!["dan@example.com".to_string()],
            bcc: vec!["hidden@example.com".to_string()],
            reply_to: vec![],
            subject: "Quarterly numbers".to_string(),
            text_body: "See chart".to_string(),
            html_body: Some(r#"<p>See chart</p><img src="cid:chart1">"#.to_string()),
            attachments: vec![Attachment {
                filename: "q3.csv".to_string(),
                content_type: "text/csv".to_string(),
                bytes: b"a,b\n1,2\n".to_vec(),
            }],
            inline_images: vec![InlineImage {
                cid: "chart1".to_string(),
                content_type: "image/png".to_string(),
                bytes: vec![0x89, b'P', b'N', b'G'],
            }],
            calendar_part: None,
            read_receipt: None,
            request_read_receipt: true,
            importance: Importance::Low,
            sensitivity: MessageSensitivity::Private,
            in_reply_to: Some("older@example.com".to_string()),
            references: vec!["older@example.com".to_string()],
            message_id: Some("draft-9@lookout.local".to_string()),
        };

        let prefill = build_outbox_prefill(&lookout_mail::send::OutboxMessage::from_composed(&msg)).expect("queued message parses");
        assert_eq!(prefill.to.as_deref(), Some("Bob <bob@example.com>"));
        assert_eq!(prefill.cc.as_deref(), Some("dan@example.com"));
        assert_eq!(prefill.bcc.as_deref(), Some("hidden@example.com"));
        assert_eq!(prefill.subject.as_deref(), Some("Quarterly numbers"));
        assert_eq!(prefill.from.as_deref(), Some("ada@example.com"));
        let html = prefill.html_body.as_deref().expect("rich body kept");
        assert!(html.contains("data:image/png;base64,"), "inline image inlined as data: URI: {html}");
        assert_eq!(prefill.attachments.len(), 1);
        assert_eq!(prefill.attachments[0].filename, "q3.csv");
        assert_eq!(prefill.importance, Importance::Low);
        assert_eq!(prefill.sensitivity, MessageSensitivity::Private);
        assert!(prefill.request_read_receipt);
        assert_eq!(prefill.in_reply_to.as_deref(), Some("older@example.com"));
        assert_eq!(prefill.draft_message_id.as_deref(), Some("draft-9@lookout.local"));
        assert!(prefill.draft_saved);
        assert!(prefill.draft_source.is_none());
    }

    fn local(y: i32, m: u32, d: u32, h: u32, min: u32) -> chrono::DateTime<chrono::FixedOffset> {
        chrono::FixedOffset::east_opt(2 * 3600).unwrap().with_ymd_and_hms(y, m, d, h, min, 0).unwrap()
    }

    /// Schedule-send presets (item 1.14): tomorrow 08:00 and 13:00, then the
    /// coming Monday 08:00 - from a Friday that's three days out.
    #[test]
    fn schedule_presets_offer_tomorrow_and_monday() {
        let presets = schedule_presets(&local(2026, 10, 16, 17, 45)); // Friday
        let got: Vec<_> = presets.iter().map(|p| (p.name, p.at)).collect();
        assert_eq!(
            got,
            vec![
                ("Tomorrow morning", local(2026, 10, 17, 8, 0)),
                ("Tomorrow afternoon", local(2026, 10, 17, 13, 0)),
                ("Monday morning", local(2026, 10, 19, 8, 0)),
            ]
        );
        // From a Monday, "Monday morning" is a week out.
        assert_eq!(schedule_presets(&local(2026, 10, 12, 9, 0))[2].at, local(2026, 10, 19, 8, 0));
    }

    /// On a Sunday tomorrow *is* Monday, so "Monday morning" would only
    /// repeat "Tomorrow morning" and is left out.
    #[test]
    fn schedule_presets_drop_monday_when_it_is_tomorrow() {
        let presets = schedule_presets(&local(2026, 10, 18, 20, 0)); // Sunday
        assert_eq!(presets.iter().map(|p| p.name).collect::<Vec<_>>(), vec!["Tomorrow morning", "Tomorrow afternoon"]);
        assert_eq!(presets[0].at, local(2026, 10, 19, 8, 0));
    }

    /// Undo send (item 1.13) reopens exactly what was about to go out:
    /// recipients including Bcc, identity, subject, bodies with the inline
    /// image back as a `data:` URI, attachments, importance, receipt request,
    /// threading - and the autosaved draft's id, flagged as already stored so
    /// the reopened composer replaces it rather than saving a second copy.
    #[test]
    fn resend_prefill_reopens_the_undone_message_verbatim() {
        let msg = ComposedMessage {
            from: "ada@example.com".to_string(),
            display_name: Some("Ada".to_string()),
            to: vec!["Bob <bob@example.com>".to_string(), "carol@example.com".to_string()],
            cc: vec!["dan@example.com".to_string()],
            bcc: vec!["hidden@example.com".to_string()],
            reply_to: vec![],
            subject: "Quarterly numbers".to_string(),
            text_body: "See chart".to_string(),
            html_body: Some(r#"<p>See chart</p><img src="cid:chart1">"#.to_string()),
            attachments: vec![Attachment {
                filename: "q3.csv".to_string(),
                content_type: "text/csv".to_string(),
                bytes: b"a,b\n1,2\n".to_vec(),
            }],
            inline_images: vec![InlineImage {
                cid: "chart1".to_string(),
                content_type: "image/png".to_string(),
                bytes: vec![0x89, b'P', b'N', b'G'],
            }],
            calendar_part: None,
            read_receipt: None,
            request_read_receipt: true,
            importance: Importance::High,
            sensitivity: MessageSensitivity::CompanyConfidential,
            in_reply_to: Some("older@example.com".to_string()),
            references: vec!["older@example.com".to_string()],
            message_id: Some("draft-9@lookout.local".to_string()),
        };

        let prefill = build_resend_prefill(&msg);
        assert_eq!(prefill.to.as_deref(), Some("Bob <bob@example.com>, carol@example.com"));
        assert_eq!(prefill.cc.as_deref(), Some("dan@example.com"));
        assert_eq!(prefill.bcc.as_deref(), Some("hidden@example.com"));
        assert_eq!(prefill.subject.as_deref(), Some("Quarterly numbers"));
        assert_eq!(prefill.from.as_deref(), Some("ada@example.com"));
        let html = prefill.html_body.as_deref().expect("rich body kept");
        assert!(html.contains("data:image/png;base64,"), "inline image inlined as data: URI: {html}");
        assert!(!html.contains("cid:"), "{html}");
        assert_eq!(prefill.attachments.len(), 1);
        assert_eq!(prefill.attachments[0].filename, "q3.csv");
        assert_eq!(prefill.attachments[0].bytes, b"a,b\n1,2\n");
        assert_eq!(prefill.importance, Importance::High);
        assert_eq!(prefill.sensitivity, MessageSensitivity::CompanyConfidential);
        assert!(prefill.request_read_receipt);
        assert_eq!(prefill.in_reply_to.as_deref(), Some("older@example.com"));
        assert_eq!(prefill.references, vec!["older@example.com".to_string()]);
        assert_eq!(prefill.draft_message_id.as_deref(), Some("draft-9@lookout.local"));
        assert!(prefill.draft_saved);
        assert!(prefill.draft_source.is_none());

        // Never autosaved: no stored draft to replace.
        let unsaved = build_resend_prefill(&ComposedMessage { message_id: None, ..msg });
        assert!(unsaved.draft_message_id.is_none());
        assert!(!unsaved.draft_saved);
    }

    /// The identity's automatic Bcc is added once, even when the list
    /// already carries it (a reopened draft or undone send).
    #[test]
    fn identity_bcc_is_not_duplicated() {
        let mut identity = Identity::new(AccountId("/test/account".to_string()), "Ada", "ada@example.com");
        identity.bcc = vec![EmailAddress {
            name: None,
            address: "Archive@Example.com".to_string(),
        }];
        assert_eq!(
            with_identity_bcc(vec!["x@example.com".to_string()], &identity),
            vec!["x@example.com".to_string(), "Archive@Example.com".to_string()]
        );
        assert_eq!(
            with_identity_bcc(vec!["archive@example.com".to_string()], &identity),
            vec!["archive@example.com".to_string()]
        );
    }

    #[test]
    fn subject_prefix_is_not_doubled_when_already_present() {
        assert_eq!(with_prefix_once("Hello", "Re: "), "Re: Hello");
        assert_eq!(with_prefix_once("Re: Hello", "Re: "), "Re: Hello");
        assert_eq!(with_prefix_once("re: Hello", "Re: "), "re: Hello");
    }

    #[test]
    fn reply_threading_strips_angle_brackets_and_appends_original_id() {
        let headers = vec![
            ("Message-ID".to_string(), "<orig@example.com>".to_string()),
            ("References".to_string(), "<older@example.com> <older2@example.com>".to_string()),
        ];
        let body = sample_body(headers, Some("hi"));
        let (in_reply_to, references) = reply_threading(&body);
        assert_eq!(in_reply_to, Some("orig@example.com".to_string()));
        assert_eq!(
            references,
            vec!["older@example.com".to_string(), "older2@example.com".to_string(), "orig@example.com".to_string()]
        );
    }

    #[test]
    fn reply_threading_with_no_references_header_falls_back_to_just_message_id() {
        let headers = vec![("Message-ID".to_string(), "<orig@example.com>".to_string())];
        let body = sample_body(headers, Some("hi"));
        let (in_reply_to, references) = reply_threading(&body);
        assert_eq!(in_reply_to, Some("orig@example.com".to_string()));
        assert_eq!(references, vec!["orig@example.com".to_string()]);
    }

    #[test]
    fn reply_excludes_other_recipients_reply_all_includes_them_minus_own_address() {
        let summary = sample_summary();
        let body = sample_body(vec![("Message-ID".to_string(), "<orig@example.com>".to_string())], Some("original text"));

        let reply = build_reply_prefill(&summary, &body, "me@example.com", ReplyMode::Reply);
        assert_eq!(reply.to.as_deref(), Some("ada@example.com"));
        assert_eq!(reply.cc, None);

        let reply_all = build_reply_prefill(&summary, &body, "me@example.com", ReplyMode::ReplyAll);
        assert_eq!(reply_all.to.as_deref(), Some("ada@example.com, other@example.com"));
        assert_eq!(reply_all.cc.as_deref(), Some("cc-person@example.com"));
    }

    #[test]
    fn forward_prefill_has_no_threading_and_blank_recipients() {
        let summary = sample_summary();
        let body = sample_body(vec![("Message-ID".to_string(), "<orig@example.com>".to_string())], Some("original text"));
        let forward = build_forward_prefill(&summary, &body);
        assert_eq!(forward.to, None);
        assert_eq!(forward.cc, None);
        assert_eq!(forward.in_reply_to, None);
        assert!(forward.references.is_empty());
        assert_eq!(forward.subject.as_deref(), Some("Fwd: Hello there"));
        assert!(forward.body.unwrap().contains("original text"));
        assert!(forward.html_body.is_none(), "a plain-text-only original forwards with no HTML companion");
    }

    #[test]
    fn forward_prefill_carries_the_original_html_with_its_cid_refs() {
        let summary = sample_summary();
        let mut body = sample_body(Vec::new(), Some("original text"));
        body.html_body = Some("<p>Hi <img src=\"cid:logo@lookout\"></p>".to_string());
        let forward = build_forward_prefill(&summary, &body);
        let html = forward.html_body.expect("html companion");
        assert!(html.contains("---------- Forwarded message ----------"), "header block present: {html}");
        assert!(html.contains("blockquote"), "original HTML is quoted: {html}");
        // The `cid:` reference survives verbatim so the launcher can resolve it
        // to a `data:` URI once the inline part has been fetched.
        assert!(html.contains("cid:logo@lookout"), "cid ref preserved for the launcher: {html}");
        assert!(!html.contains("Fwd:"), "header names the original subject, not the Fwd: one: {html}");
    }

    #[test]
    fn forward_as_attachment_prefill_attaches_the_raw_message() {
        let summary = sample_summary();
        let raw = b"From: ada@example.com\r\nSubject: Hi\r\n\r\nbody";
        let forward = build_forward_attachment_prefill(&summary, raw);
        assert_eq!(forward.subject.as_deref(), Some("Fwd: Hello there"));
        assert_eq!(forward.attachments.len(), 1);
        assert_eq!(forward.attachments[0].content_type, "message/rfc822");
        assert_eq!(forward.attachments[0].filename, "Hello there.eml");
        assert_eq!(forward.attachments[0].bytes, raw.to_vec());
        // Nothing is quoted inline - the attachment carries the original.
        assert!(forward.body.as_deref().unwrap_or("").trim().is_empty());
        assert!(forward.html_body.is_none());
    }

    #[test]
    fn text_to_html_escapes_special_characters() {
        assert_eq!(text_to_html("a < b & c > d"), "<p>a &lt; b &amp; c &gt; d</p>");
        assert_eq!(text_to_html("\"quoted\" and 'apostrophe'"), "<p>&quot;quoted&quot; and &#39;apostrophe&#39;</p>");
    }

    #[test]
    fn text_to_html_quotes_become_blockquote() {
        assert_eq!(text_to_html("> quoted\n> still quoted"), "<blockquote>quoted<br>still quoted</blockquote>");
        assert_eq!(text_to_html("> one"), "<blockquote>one</blockquote>");
    }

    #[test]
    fn text_to_html_paragraphs_and_breaks() {
        assert_eq!(text_to_html("first line\nsecond line\n\nnew para"), "<p>first line<br>second line</p><p>new para</p>");
        assert_eq!(text_to_html("just one"), "<p>just one</p>");
    }

    #[test]
    fn text_to_html_horizontal_rules() {
        assert_eq!(text_to_html("---"), "<hr>");
        assert_eq!(text_to_html("text\n\n***"), "<p>text</p><hr>");
        assert_eq!(text_to_html("___"), "<hr>");
        assert_eq!(text_to_html("--"), "<p>--</p>");
    }

    #[test]
    fn text_to_html_empty_and_blank() {
        assert_eq!(text_to_html(""), "");
        assert_eq!(text_to_html("\n\n"), "<p><br></p><p><br></p>");
    }

    #[test]
    fn text_to_html_mixed_quote_and_text() {
        assert_eq!(
            text_to_html("intro\n\n> quoted line\n> second quote"),
            "<p>intro</p><blockquote>quoted line<br>second quote</blockquote>"
        );
    }

    #[test]
    fn parse_recipients_splits_trims_and_drops_blanks() {
        assert_eq!(parse_recipients("a@example.com, b@example.com"), vec!["a@example.com", "b@example.com"]);
        assert_eq!(parse_recipients("  only@example.com  "), vec!["only@example.com"]);
        assert_eq!(parse_recipients("a@example.com,, ,b@example.com,"), vec!["a@example.com", "b@example.com"]);
        assert!(parse_recipients("").is_empty());
        assert!(parse_recipients(" , ").is_empty());
        // Delegating to the chip tokenizer is what keeps a "Surname, Given"
        // display name one recipient instead of two broken ones.
        assert_eq!(parse_recipients("\"Lovelace, Ada\" <ada@example.com>"), vec!["\"Lovelace, Ada\" <ada@example.com>"]);
    }

    fn snapshot(to: &str, subject: &str, body_text: &str) -> DraftSnapshot {
        DraftSnapshot {
            from: "me@example.com".to_string(),
            to: to.to_string(),
            cc: String::new(),
            bcc: String::new(),
            subject: subject.to_string(),
            rich: false,
            read_receipt: false,
            importance: Importance::Normal,
            sensitivity: MessageSensitivity::Normal,
            body: body_text.to_string(),
            body_text: body_text.to_string(),
            attachments: vec![],
            inline_images: vec![],
        }
    }

    #[test]
    fn empty_composer_is_trivial_and_never_autosaved() {
        assert!(draft_is_trivial(&snapshot("", "", "")));
        assert!(draft_is_trivial(&snapshot("  ", "", "\n  ")));
    }

    #[test]
    fn any_recipient_subject_or_body_makes_a_draft_worth_saving() {
        assert!(!draft_is_trivial(&snapshot("a@example.com", "", "")));
        assert!(!draft_is_trivial(&snapshot("", "subject", "")));
        assert!(!draft_is_trivial(&snapshot("", "", "body")));
        // A blind copy is the whole message for some drafts - it must count
        // as content, or a Bcc-only composer would never autosave.
        let mut bcc_only = snapshot("", "", "");
        bcc_only.bcc = "hidden@example.com".to_string();
        assert!(!draft_is_trivial(&bcc_only));
    }

    #[test]
    fn an_attachment_or_inline_image_alone_makes_a_draft_worth_saving() {
        let mut with_attachment = snapshot("", "", "");
        with_attachment.attachments = vec![PendingAttachment {
            filename: "photo.png".to_string(),
            content_type: "image/png".to_string(),
            bytes: Rc::new(vec![0u8; 4]),
        }];
        assert!(!draft_is_trivial(&with_attachment));

        let mut with_image = snapshot("", "", "");
        with_image.inline_images = vec![InlineImage {
            cid: "img-1@lookout.local".to_string(),
            content_type: "image/png".to_string(),
            bytes: vec![0u8; 4],
        }];
        assert!(!draft_is_trivial(&with_image));
    }

    #[test]
    fn rich_blank_document_is_trivial() {
        // The rich editor's untouched document renders to whitespace-only
        // text even though its HTML is non-empty.
        let snap = DraftSnapshot {
            from: String::new(),
            to: String::new(),
            cc: String::new(),
            bcc: String::new(),
            subject: String::new(),
            rich: true,
            read_receipt: false,
            importance: Importance::Normal,
            sensitivity: MessageSensitivity::Normal,
            body: "<p><br></p>".to_string(),
            body_text: "\n".to_string(),
            attachments: vec![],
            inline_images: vec![],
        };
        assert!(draft_is_trivial(&snap));
    }

    #[test]
    fn attachment_reminder_matches_mention_but_not_detached_files() {
        assert!(looks_like_missing_attachment("", "Please see the attached report."));
        assert!(looks_like_missing_attachment("", "As per the attachment, ..."));
        assert!(looks_like_missing_attachment("Docs enclosed", ""));
        assert!(looks_like_missing_attachment("", "RE-ATTACHED for reference"));
        assert!(!looks_like_missing_attachment("", "I detached the cable."));
        assert!(!looks_like_missing_attachment("Meeting notes", "No files here."));
    }
}
