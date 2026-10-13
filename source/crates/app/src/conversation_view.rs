//! The reading pane's conversation view (item 1.7, M-04 / UX-35): every
//! message of a conversation stacked as cards in one WebView, newest on top
//! or bottom, older ones collapsed to sender and a one-line preview.
//!
//! This module is the pure half - which messages make up a conversation,
//! which start expanded, and the HTML document and script calls that draw
//! them. `window.rs` owns the WebView and wires the script's messages to the
//! app's verbs (reply, the message menu, attachments, remote content).
//!
//! **Isolation.** Each message body sits in its own `<iframe sandbox
//! srcdoc>` carrying the same CSP `wrap_message_with_csp` gives the single-
//! message view, so remote content stays per sender (a frame's CSP can only
//! narrow the document's) and one sender's styles never leak into another
//! card. Page scripts never run: the WebView has `enable-javascript-markup`
//! off, every CSP says `script-src 'none'`, and the sandbox withholds
//! `allow-scripts`. The only script is [`SCRIPT`], a trusted user script in
//! its own world ([`SCRIPT_WORLD`]): it sizes the frames, toggles cards, and
//! posts clicks to the app through the [`SCRIPT_HANDLER`] message handler.
//!
//! **Links.** A sandboxed frame without `allow-scripts` runs no event
//! listeners at all, and the parent CSP's `frame-src 'none'` silently drops
//! an in-frame navigation. So the script, injected into every frame,
//! retargets each link to `_blank`: a click then arrives as a new-window
//! decision, which the WebView's `decide-policy` hands to the browser.

/// Copyright (C) <2026>  <Gavin Graham & Contributors>
/// Software released under the GPL3 license
use std::collections::HashSet;

use chrono::{DateTime, Utc};
use lookout_core::{EmailBody, EmailSummary, Importance, MailboxId, SystemFlagBit, Uid};

use crate::settings::ConversationOrder;

/// The user-script world the conversation script runs in, isolated from
/// the (script-free) page.
pub const SCRIPT_WORLD: &str = "lookout";

/// The script message handler the conversation script posts to.
pub const SCRIPT_HANDLER: &str = "lookout";

/// The trusted conversation script (see the module doc).
pub const SCRIPT: &str = include_str!("conversation_view.js");

/// What a conversation `cid:` reference is prefixed with, so the one `cid`
/// URI scheme handler the reading pane's WebViews share can tell a card's
/// inline image (`cid:lkc3~image001.png@…`) from the single-message view's.
const CID_PREFIX: &str = "lkc";

/// One message of a conversation, with the folder label shown when it comes
/// from somewhere other than the listed folder (Sent Items).
#[derive(Clone, Debug)]
pub struct Member {
    pub summary: EmailSummary,
    pub folder_label: Option<String>,
}

impl Member {
    pub fn key(&self) -> (MailboxId, Uid) {
        (self.summary.mailbox.clone(), self.summary.uid)
    }
}

/// An attachment tile on a card.
#[derive(Clone, Debug, PartialEq)]
pub struct AttachmentTile {
    pub part_number: String,
    pub name: String,
    pub size_label: String,
}

/// One card of the rendered conversation, in display order.
#[derive(Clone, Debug)]
pub struct Card {
    pub member: Member,
    pub expanded: bool,
    /// The body's `srcdoc` document, once the body has loaded.
    pub srcdoc: Option<String>,
    /// Remote content the body references that its sender's policy blocks.
    pub remote_blocked: bool,
    /// The message asks for a read receipt, which only the single-message
    /// view (its banner) answers.
    pub wants_receipt: bool,
    pub attachments: Vec<AttachmentTile>,
}

impl Card {
    pub fn key(&self) -> (MailboxId, Uid) {
        self.member.key()
    }
}

/// A message posted by the conversation script.
#[derive(Clone, Debug, PartialEq)]
pub enum ScriptMessage {
    /// A collapsed card was opened: load its body, count it as read.
    Expand(usize),
    Collapse(usize),
    Reply(usize),
    ReplyAll(usize),
    Forward(usize),
    /// The card's "More actions" button, at window-relative `(x, y)` of the
    /// WebView.
    Menu {
        card: usize,
        x: f64,
        y: f64,
    },
    /// An attachment tile was clicked.
    Attachment {
        card: usize,
        part: String,
        x: f64,
        y: f64,
    },
    /// "Show blocked content" on a card.
    ShowBlocked(usize),
    /// "Show details" on a card whose message has an invitation, a
    /// read-receipt request or an unsubscribe link: open it on its own.
    OpenDetails(usize),
    /// The card's sender name/avatar was clicked: open the profile card
    /// (Phase 4.2 / UX-67) at window-relative `(x, y)` of the WebView.
    Sender {
        card: usize,
        x: f64,
        y: f64,
    },
}

/// Parses a script message (`{"action": "...", "card": n, ...}`); `None`
/// for anything malformed or unknown.
pub fn parse_script_message(json: &str) -> Option<ScriptMessage> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let card = value.get("card")?.as_u64()? as usize;
    let x = value.get("x").and_then(|v| v.as_f64()).unwrap_or(0.0);
    let y = value.get("y").and_then(|v| v.as_f64()).unwrap_or(0.0);
    Some(match value.get("action")?.as_str()? {
        "expand" => ScriptMessage::Expand(card),
        "collapse" => ScriptMessage::Collapse(card),
        "reply" => ScriptMessage::Reply(card),
        "reply-all" => ScriptMessage::ReplyAll(card),
        "forward" => ScriptMessage::Forward(card),
        "menu" => ScriptMessage::Menu { card, x, y },
        "attachment" => ScriptMessage::Attachment {
            card,
            part: value.get("part")?.as_str()?.to_string(),
            x,
            y,
        },
        "show-blocked" => ScriptMessage::ShowBlocked(card),
        "open-details" => ScriptMessage::OpenDetails(card),
        "sender" => ScriptMessage::Sender { card, x, y },
        _ => return None,
    })
}

fn bare_id(id: &str) -> &str {
    id.trim().trim_start_matches('<').trim_end_matches('>')
}

/// Every Message-ID a conversation's messages name - their own, the ones
/// they reply to, and their References - the set its members elsewhere
/// (Sent Items) are matched against.
pub fn conversation_ids(messages: &[EmailSummary]) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for message in messages {
        let ids = message.message_id.iter().chain(message.in_reply_to.iter()).chain(message.references.iter());
        for id in ids {
            let id = bare_id(id);
            if !id.is_empty() && seen.insert(id.to_string()) {
                out.push(id.to_string());
            }
        }
    }
    out
}

/// Merges the listed conversation with its messages from another folder
/// (labelled `other_label`), oldest first. A message present in both (a
/// copy, or mail to yourself) shows once, as the listed folder's.
pub fn merge_members(listed: Vec<EmailSummary>, other: Vec<EmailSummary>, other_label: &str) -> Vec<Member> {
    let mut seen_ids: HashSet<String> = listed.iter().filter_map(|m| m.message_id.as_deref()).map(|id| bare_id(id).to_string()).collect();
    let mut seen_keys: HashSet<(MailboxId, Uid)> = listed.iter().map(|m| (m.mailbox.clone(), m.uid)).collect();
    let mut members: Vec<Member> = listed.into_iter().map(|summary| Member { summary, folder_label: None }).collect();
    for summary in other {
        let id = summary.message_id.as_deref().map(bare_id).filter(|id| !id.is_empty()).map(str::to_string);
        if id.as_ref().is_some_and(|id| seen_ids.contains(id)) || !seen_keys.insert((summary.mailbox.clone(), summary.uid)) {
            continue;
        }
        if let Some(id) = id {
            seen_ids.insert(id);
        }
        members.push(Member {
            summary,
            folder_label: Some(other_label.to_string()),
        });
    }
    members.sort_by(|a, b| a.summary.date.cmp(&b.summary.date).then_with(|| a.summary.uid.0.cmp(&b.summary.uid.0)));
    members
}

/// Puts oldest-first `members` into display order.
pub fn display_order(mut members: Vec<Member>, order: ConversationOrder) -> Vec<Member> {
    if order == ConversationOrder::NewestOnTop {
        members.reverse();
    }
    members
}

/// Which cards start expanded, as in Outlook: the newest message, every
/// unread one, and the one selected in the list (`focus`).
pub fn initially_expanded(members: &[Member], focus: Option<&(MailboxId, Uid)>) -> HashSet<(MailboxId, Uid)> {
    let mut expanded: HashSet<(MailboxId, Uid)> = members.iter().filter(|m| m.summary.is_unread()).map(Member::key).collect();
    if let Some(newest) = members
        .iter()
        .max_by(|a, b| a.summary.date.cmp(&b.summary.date).then_with(|| a.summary.uid.0.cmp(&b.summary.uid.0)))
    {
        expanded.insert(newest.key());
    }
    if let Some(focus) = focus {
        expanded.insert(focus.clone());
    }
    expanded
}

/// Rewrites a body's `cid:` references to name their card
/// (`cid:lkc{card}~…`), the form [`split_card_cid`] reverses.
pub fn qualify_cid_refs(html: &str, card: usize) -> String {
    let mut out = String::with_capacity(html.len() + 16);
    let mut rest = html;
    while let Some(start) = find_ascii_case_insensitive(rest, "cid:") {
        out.push_str(&rest[..start]);
        out.push_str("cid:");
        out.push_str(&format!("{CID_PREFIX}{card}~"));
        rest = &rest[start + 4..];
    }
    out.push_str(rest);
    out
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let hay = haystack.as_bytes();
    let needle = needle.as_bytes();
    (0..hay.len().checked_sub(needle.len())? + 1).find(|&i| hay[i..i + needle.len()].eq_ignore_ascii_case(needle))
}

/// Splits a conversation `cid:` reference (the URI's path) into its card
/// index and the message's own reference; `None` for a single-message
/// view's reference.
pub fn split_card_cid(reference: &str) -> Option<(usize, &str)> {
    let rest = reference.strip_prefix(CID_PREFIX)?;
    let (index, cid) = rest.split_once('~')?;
    Some((index.parse().ok()?, cid))
}

/// The `srcdoc` document for a card's body: the HTML body (or the escaped
/// plain text), its `cid:` references qualified for card `card`, wrapped in
/// the reading pane's CSP for the sender's policy. `dark` applies the
/// "Switch message theme" override, as the single-message view does.
pub fn body_srcdoc(body: &EmailBody, card: usize, images_allowed: bool, all_allowed: bool, dark: bool) -> String {
    let html = match (&body.html_body, &body.text_body) {
        (Some(html), _) => html.clone(),
        (None, Some(text)) => format!("<pre style=\"white-space:pre-wrap;font-family:inherit;margin:0\">{}</pre>", escape_html(text)),
        (None, None) => String::new(),
    };
    let theme = if dark { crate::window::MESSAGE_THEME_OVERRIDE_CSS } else { "" };
    let html = format!("{}<style>{BODY_BASE_CSS}{theme}</style>", qualify_cid_refs(&html, card));
    crate::window::wrap_message_with_csp(&html, images_allowed, all_allowed)
}

/// Base style inside every body frame: no outer margin (the card pads it),
/// and wide content scrolls within its card instead of widening the page.
const BODY_BASE_CSS: &str = "html,body{margin:0;padding:0}body{overflow-x:auto;overflow-wrap:anywhere}img{max-width:100%;height:auto}";

pub fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(ch),
        }
    }
    out
}

/// The document-level CSP: our own markup and styles only. Frames inherit
/// it, so it allows remote images, fonts and media as far as the most
/// trusting card needs; each frame's own CSP then narrows that to its
/// sender's policy.
fn document_csp(cards: &[Card]) -> &'static str {
    let any_all = cards.iter().any(|c| c.srcdoc.as_deref().is_some_and(|d| d.contains("default-src *")));
    if any_all {
        "default-src * data: cid: blob:; style-src 'unsafe-inline' * data: cid:; script-src 'none'; frame-src 'none'; object-src 'none'; connect-src * data:; base-uri 'none'; form-action 'none'"
    } else {
        "default-src 'none'; img-src * data: cid:; style-src 'unsafe-inline'; font-src data: cid:; media-src data: cid:; frame-src 'none'; object-src 'none'; connect-src 'none'; script-src 'none'; base-uri 'none'; form-action 'none'"
    }
}

/// The whole conversation document: a toolbar with the message count and
/// Expand all / Collapse all, then every card in display order. (The
/// subject sits in the pane's native header.)
pub fn build_document(cards: &[Card], now: DateTime<Utc>, dark: bool) -> String {
    let mut html = String::new();
    html.push_str("<!doctype html><html><head><meta charset=\"utf-8\">");
    html.push_str(&format!("<meta http-equiv=\"Content-Security-Policy\" content=\"{}\">", document_csp(cards)));
    html.push_str("<style>");
    html.push_str(DOCUMENT_CSS);
    html.push_str(if dark { "</style></head><body class=\"dark\">" } else { "</style></head><body>" });
    html.push_str(&format!(
        "<div class=\"toolbar\"><span class=\"count\">{}</span><button type=\"button\" class=\"link\" data-action=\"expand-all\">Expand all</button><button type=\"button\" class=\"link\" data-action=\"collapse-all\">Collapse all</button></div>",
        if cards.len() == 1 { "1 message".to_string() } else { format!("{} messages", cards.len()) }
    ));
    html.push_str("<main id=\"cards\">");
    for (index, card) in cards.iter().enumerate() {
        html.push_str(&card_html(index, card, now));
    }
    html.push_str("</main></body></html>");
    html
}

/// One card's markup - also what the script swaps in when a card changes
/// (`lookout.updateCard`).
pub fn card_html(index: usize, card: &Card, now: DateTime<Utc>) -> String {
    let summary = &card.member.summary;
    let (name, address) = match summary.from.first() {
        Some(sender) => (sender.display_label().to_string(), sender.address.clone()),
        None => ("Unknown sender".to_string(), String::new()),
    };
    let initials = crate::message_header::initials(summary.from.first().and_then(|a| a.name.as_deref()), &address);
    let color = crate::message_header::avatar_color_class(&address);
    let mut classes = vec!["card"];
    if card.expanded {
        classes.push("expanded");
    }
    if summary.is_unread() {
        classes.push("unread");
    }
    let mut out = format!("<article class=\"{}\" id=\"card-{index}\" data-card=\"{index}\">", classes.join(" "));
    out.push_str(&format!(
        "<header class=\"head\" data-action=\"toggle\" tabindex=\"0\" role=\"button\" aria-expanded=\"{}\">",
        card.expanded
    ));
    out.push_str(&format!(
        "<span class=\"avatar {color}\" data-action=\"sender\" aria-hidden=\"true\">{}</span>",
        escape_html(&initials)
    ));
    out.push_str("<div class=\"who\"><div class=\"line\">");
    out.push_str(&format!(
        "<span class=\"sender\" data-action=\"sender\" title=\"{}\">{}</span>",
        escape_html(&address),
        escape_html(&name)
    ));
    out.push_str("<span class=\"chips\">");
    for chip in chips(card, now) {
        out.push_str(&format!("<span class=\"chip {}\">{}</span>", chip.0, escape_html(&chip.1)));
    }
    out.push_str("</span>");
    out.push_str(&format!(
        "<time class=\"date\" title=\"{}\">{}</time>",
        escape_html(&summary.date.with_timezone(&chrono::Local).format("%a %d/%m/%Y %I:%M %p").to_string()),
        escape_html(&crate::message_list::format_row_date(summary.date, now))
    ));
    out.push_str("</div>");
    out.push_str(&format!("<div class=\"recipients\">{}</div>", escape_html(&recipients_line(summary))));
    out.push_str(&format!("<div class=\"preview\">{}</div>", escape_html(summary.preview.as_deref().unwrap_or(""))));
    out.push_str("</div></header>");
    out.push_str("<div class=\"content\">");
    out.push_str(&format!("<div class=\"extras\">{}</div>", card_extras_html(card)));
    match &card.srcdoc {
        Some(srcdoc) => out.push_str(&frame_html(srcdoc)),
        None => out.push_str("<div class=\"loading\">Loading…</div>"),
    }
    out.push_str("<div class=\"actions\">");
    let reply_enabled = !summary.flags.contains(&SystemFlagBit::Draft);
    for (action, label) in [("reply", "Reply"), ("reply-all", "Reply all"), ("forward", "Forward")] {
        out.push_str(&format!(
            "<button type=\"button\" data-action=\"{action}\"{}>{label}</button>",
            if reply_enabled { "" } else { " disabled" }
        ));
    }
    out.push_str("<button type=\"button\" class=\"more\" data-action=\"menu\" title=\"More actions\" aria-label=\"More actions\">⋯</button>");
    out.push_str("</div></div></article>");
    out
}

/// The notes and attachment tiles above a card's body - refreshed by the
/// script's `setBody` when the body (and with it the attachment list and
/// the remote-content scan) arrives.
pub fn card_extras_html(card: &Card) -> String {
    let summary = &card.member.summary;
    let mut out = String::new();
    if card.remote_blocked {
        out.push_str(
            "<div class=\"note\">Some content in this message has been blocked to protect your privacy. <button type=\"button\" class=\"link\" data-action=\"show-blocked\">Show blocked content</button></div>",
        );
    }
    if summary.has_calendar {
        out.push_str(
            "<div class=\"note\">This message contains a meeting invitation. <button type=\"button\" class=\"link\" data-action=\"open-details\">Show invitation</button></div>",
        );
    } else if card.wants_receipt {
        out.push_str("<div class=\"note\">The sender asked for a read receipt. <button type=\"button\" class=\"link\" data-action=\"open-details\">Show details</button></div>");
    }
    if !card.attachments.is_empty() {
        out.push_str("<div class=\"attachments\">");
        for tile in &card.attachments {
            out.push_str(&format!(
                "<button type=\"button\" class=\"tile\" data-action=\"attachment\" data-part=\"{}\" title=\"Open or save {}\"><span class=\"tile-name\">{}</span><span class=\"tile-size\">{}</span></button>",
                escape_html(&tile.part_number),
                escape_html(&tile.name),
                escape_html(&tile.name),
                escape_html(&tile.size_label)
            ));
        }
        out.push_str("</div>");
    }
    out
}

/// A body frame (the script's `setBody` builds the same one).
/// `allow-same-origin` lets the trusted script measure the frame and
/// retarget its links; `allow-popups` lets a retargeted link raise the
/// new-window decision the app turns into a browser launch. Without
/// `allow-scripts` nothing in the body can run.
///
/// The body travels in `data-srcdoc`, not `srcdoc`: with markup JavaScript
/// off, WebKit's parser drops a `srcdoc` attribute, so the script moves it
/// into the frame's `srcdoc` property once the document has loaded (which
/// WebKit honours).
fn frame_html(srcdoc: &str) -> String {
    format!(
        "<iframe class=\"body\" title=\"Message body\" sandbox=\"allow-same-origin allow-popups\" scrolling=\"no\" data-srcdoc=\"{}\"></iframe>",
        escape_attribute(srcdoc)
    )
}

/// Escapes a value for a double-quoted attribute: quotes and ampersands are
/// what matter, and angle brackets go too, so even a reader of the markup
/// can't mistake a body's `</iframe>` for structure.
fn escape_attribute(value: &str) -> String {
    escape_html(value)
}

fn recipients_line(summary: &EmailSummary) -> String {
    let join = |list: &[lookout_core::EmailAddress]| list.iter().map(|a| a.display_label().to_string()).collect::<Vec<_>>().join(", ");
    let mut line = format!("To: {}", join(&summary.to));
    if !summary.cc.is_empty() {
        line.push_str(&format!("; Cc: {}", join(&summary.cc)));
    }
    line
}

/// The chips after a card's sender: folder, draft, importance, follow-up,
/// attachment count, as `(class, text)`.
fn chips(card: &Card, now: DateTime<Utc>) -> Vec<(&'static str, String)> {
    let summary = &card.member.summary;
    let mut out = Vec::new();
    if let Some(label) = &card.member.folder_label {
        out.push(("folder", label.clone()));
    }
    if summary.flags.contains(&SystemFlagBit::Draft) {
        out.push(("draft", "Draft".to_string()));
    }
    match summary.importance {
        Importance::High => out.push(("important", "High importance".to_string())),
        Importance::Low => out.push(("low", "Low importance".to_string())),
        Importance::Normal => {}
    }
    if let Some(flag) = summary.follow_up() {
        let today = now.with_timezone(&chrono::Local).date_naive();
        let text = match (flag.status, crate::follow_up::due_date(&flag)) {
            (lookout_core::FlagStatus::Complete, _) => "Completed".to_string(),
            (_, Some(due)) => crate::follow_up::due_label(due, today),
            (_, None) => "Flagged".to_string(),
        };
        out.push(("flag", text));
    }
    let attachments = if card.attachments.is_empty() && summary.has_attachment {
        1
    } else {
        card.attachments.len()
    };
    if attachments > 0 {
        out.push((
            "attachment",
            if attachments == 1 {
                "1 attachment".to_string()
            } else {
                format!("{attachments} attachments")
            },
        ));
    }
    out
}

/// Script call: put card `card`'s loaded body (its `srcdoc` document) in
/// place, with its notes and attachment tiles.
pub fn js_set_body(card: &Card, index: usize) -> String {
    format!(
        "lookout.setBody({index}, {}, {})",
        serde_json::to_string(card.srcdoc.as_deref().unwrap_or("")).unwrap_or_default(),
        serde_json::to_string(&card_extras_html(card)).unwrap_or_default()
    )
}

/// Script call: card `card`'s body couldn't be loaded.
pub fn js_body_failed(card: usize) -> String {
    format!("lookout.bodyFailed({card})")
}

/// Script call: open or close card `card` (the app's side of a toggle, e.g.
/// the list selecting a collapsed message), without echoing it back.
pub fn js_set_expanded(card: usize, expanded: bool) -> String {
    format!("lookout.setExpanded({card}, {expanded})")
}

/// Script call: replace card `card`'s markup (flag, read state or category
/// changed), keeping its loaded frame.
pub fn js_update_card(card: usize, html: &str) -> String {
    format!("lookout.updateCard({card}, {})", serde_json::to_string(html).unwrap_or_default())
}

/// Script call: scroll card `card` into view.
pub fn js_scroll_to(card: usize) -> String {
    format!("lookout.scrollTo({card})")
}

/// Script call: the vertical scroll offset, to carry over a rebuild.
pub const JS_SCROLL_Y: &str = "window.scrollY";

/// Script call: restore a scroll offset after a rebuild.
pub fn js_restore_scroll(y: f64) -> String {
    format!("lookout.restoreScroll({y})")
}

/// The conversation document's own look: libadwaita-like cards in light
/// and dark, following the system scheme.
const DOCUMENT_CSS: &str = r#"
:root { color-scheme: light dark; --bg: #fafafb; --card: #ffffff; --fg: #1e1e20; --dim: #6b6b70; --line: rgba(0,0,0,.09); --accent: #3584e4; --chip: rgba(0,0,0,.06); --note: #fdf4d7; }
@media (prefers-color-scheme: dark) { :root { --bg: #222226; --card: #2e2e32; --fg: #f0f0f2; --dim: #a0a0a8; --line: rgba(255,255,255,.1); --accent: #78aeed; --chip: rgba(255,255,255,.1); --note: #4a4120; } }
html, body { margin: 0; background: var(--bg); color: var(--fg); font: 14px/1.4 system-ui, sans-serif; }
body { padding: 8px 12px 16px; }
.toolbar { display: flex; align-items: center; gap: 12px; padding: 0 4px 8px; color: var(--dim); font-size: 12px; }
.toolbar .count { flex: 1; }
button { font: inherit; color: inherit; }
button.link { background: none; border: 0; padding: 0; color: var(--accent); cursor: pointer; }
button.link:hover { text-decoration: underline; }
.card { background: var(--card); border: 1px solid var(--line); border-radius: 10px; margin: 0 0 8px; overflow: hidden; }
.card.unread { box-shadow: inset 3px 0 0 var(--accent); }
.head { display: flex; gap: 10px; align-items: flex-start; padding: 10px 12px; cursor: pointer; outline: none; }
.head:focus-visible { box-shadow: inset 0 0 0 2px var(--accent); border-radius: 10px; }
.avatar { flex: none; width: 32px; height: 32px; border-radius: 50%; display: flex; align-items: center; justify-content: center; color: #fff; font-weight: 600; font-size: 12px; }
.avatar-color-0 { background: #3584e4; } .avatar-color-1 { background: #2ec27e; } .avatar-color-2 { background: #e66100; }
.avatar-color-3 { background: #9141ac; } .avatar-color-4 { background: #c01c28; } .avatar-color-5 { background: #1c71d8; }
.who { flex: 1; min-width: 0; }
.line { display: flex; align-items: baseline; gap: 6px; }
.sender { font-weight: 600; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.unread .sender { color: var(--accent); }
.chips { flex: 1; display: flex; gap: 4px; flex-wrap: wrap; min-width: 0; }
.chip { font-size: 11px; padding: 0 6px; border-radius: 8px; background: var(--chip); color: var(--dim); white-space: nowrap; }
.chip.important { color: #c01c28; } .chip.draft { color: #c01c28; } .chip.flag { color: #c01c28; }
.date { flex: none; color: var(--dim); font-size: 12px; }
.recipients, .preview { color: var(--dim); font-size: 12px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.card:not(.expanded) .recipients, .card.expanded .preview { display: none; }
.content { display: none; padding: 0 12px 10px 54px; }
.card.expanded .content { display: block; }
.note { background: var(--note); border-radius: 6px; padding: 6px 10px; margin: 0 0 8px; font-size: 12px; }
.attachments { display: flex; flex-wrap: wrap; gap: 6px; margin: 0 0 8px; }
.tile { display: flex; flex-direction: column; align-items: flex-start; max-width: 220px; padding: 6px 10px; border: 1px solid var(--line); border-radius: 6px; background: var(--card); cursor: pointer; }
.tile:hover { border-color: var(--accent); }
.tile-name { max-width: 200px; white-space: nowrap; overflow: hidden; text-overflow: ellipsis; }
.tile-size { color: var(--dim); font-size: 11px; }
iframe.body { display: block; width: 100%; height: 40px; border: 0; background: #fff; border-radius: 4px; }
.dark iframe.body { background: transparent; }
.loading { color: var(--dim); padding: 8px 0; }
.actions { display: flex; gap: 6px; margin-top: 10px; }
.actions button { background: var(--chip); border: 0; border-radius: 6px; padding: 4px 12px; cursor: pointer; }
.actions button:hover:not(:disabled) { background: var(--line); }
.actions button:disabled { opacity: .5; cursor: default; }
.actions .more { padding: 4px 10px; }
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;
    use lookout_core::{AccountId, EmailAddress, ThreadKey};

    fn summary(mailbox: &MailboxId, uid: u32, hour: u32, id: &str) -> EmailSummary {
        EmailSummary {
            uid: Uid(uid),
            mailbox: mailbox.clone(),
            message_id: Some(format!("<{id}>")),
            in_reply_to: None,
            references: Vec::new(),
            thread_key: ThreadKey("root".into()),
            subject: Some("Plans".into()),
            from: vec![EmailAddress {
                name: Some("Ada Lovelace".into()),
                address: "ada@example.com".into(),
            }],
            to: vec![EmailAddress {
                name: None,
                address: "bob@example.com".into(),
            }],
            cc: Vec::new(),
            date: Utc.with_ymd_and_hms(2026, 10, 12, hour, 0, 0).unwrap(),
            flags: [SystemFlagBit::Seen].into_iter().collect(),
            keywords: Default::default(),
            size: 0,
            has_attachment: false,
            has_calendar: false,
            preview: Some("See you there".into()),
            importance: Importance::Normal,
            sensitivity: Default::default(),
            list_id: None,
            structure: None,
        }
    }

    fn inbox() -> MailboxId {
        MailboxId::new(&AccountId("acct".into()), "INBOX")
    }

    fn sent() -> MailboxId {
        MailboxId::new(&AccountId("acct".into()), "Sent")
    }

    fn make_body(html: Option<&str>, text: Option<&str>) -> EmailBody {
        EmailBody {
            uid: Uid(1),
            text_body: text.map(str::to_string),
            html_body: html.map(str::to_string),
            calendar_ics: None,
            parts: Vec::new(),
            headers: Vec::new(),
            auth_results: None,
        }
    }

    fn card(member: Member, expanded: bool, srcdoc: Option<&str>) -> Card {
        Card {
            member,
            expanded,
            srcdoc: srcdoc.map(str::to_string),
            remote_blocked: false,
            wants_receipt: false,
            attachments: Vec::new(),
        }
    }

    #[test]
    fn conversation_ids_collect_own_reply_and_reference_ids_once() {
        let mut a = summary(&inbox(), 1, 9, "a@x");
        a.references = vec!["<root@x>".into()];
        let mut b = summary(&inbox(), 2, 10, "b@x");
        b.in_reply_to = Some("<a@x>".into());
        b.references = vec!["<root@x>".into(), "<a@x>".into()];
        assert_eq!(conversation_ids(&[a, b]), vec!["a@x", "root@x", "b@x"]);
    }

    #[test]
    fn merge_keeps_the_listed_copy_and_sorts_oldest_first() {
        let listed = vec![summary(&inbox(), 2, 11, "b@x"), summary(&inbox(), 1, 9, "a@x")];
        let mine = summary(&sent(), 7, 10, "mine@x");
        let duplicate = summary(&sent(), 8, 11, "b@x");
        let members = merge_members(listed, vec![mine, duplicate], "Sent Items");
        let shape: Vec<(u32, Option<&str>)> = members.iter().map(|m| (m.summary.uid.0, m.folder_label.as_deref())).collect();
        assert_eq!(shape, vec![(1, None), (7, Some("Sent Items")), (2, None)]);
    }

    #[test]
    fn display_order_follows_the_setting() {
        let members = merge_members(vec![summary(&inbox(), 1, 9, "a@x"), summary(&inbox(), 2, 10, "b@x")], Vec::new(), "Sent Items");
        let uids = |m: Vec<Member>| m.iter().map(|m| m.summary.uid.0).collect::<Vec<_>>();
        assert_eq!(uids(display_order(members.clone(), ConversationOrder::NewestOnTop)), vec![2, 1]);
        assert_eq!(uids(display_order(members, ConversationOrder::NewestOnBottom)), vec![1, 2]);
    }

    #[test]
    fn newest_unread_and_focused_messages_start_expanded() {
        let mut unread = summary(&inbox(), 2, 10, "b@x");
        unread.flags.clear();
        let members = merge_members(
            vec![summary(&inbox(), 1, 9, "a@x"), unread, summary(&inbox(), 3, 11, "c@x"), summary(&inbox(), 4, 8, "d@x")],
            Vec::new(),
            "Sent Items",
        );
        let mut expanded: Vec<u32> = initially_expanded(&members, None).iter().map(|(_, uid)| uid.0).collect();
        expanded.sort();
        assert_eq!(expanded, vec![2, 3], "the unread message and the newest");
        let focus = (inbox(), Uid(4));
        let mut expanded: Vec<u32> = initially_expanded(&members, Some(&focus)).iter().map(|(_, uid)| uid.0).collect();
        expanded.sort();
        assert_eq!(expanded, vec![2, 3, 4], "plus the row selected in the list");
    }

    #[test]
    fn cid_references_are_qualified_per_card_and_split_back() {
        let html = r#"<img src="cid:logo@x"><img src='CID:two'>"#;
        let qualified = qualify_cid_refs(html, 3);
        assert_eq!(qualified, r#"<img src="cid:lkc3~logo@x"><img src='cid:lkc3~two'>"#);
        assert_eq!(split_card_cid("lkc3~logo@x"), Some((3, "logo@x")));
        assert_eq!(split_card_cid("12.34@host"), None, "a single-message reference passes through");
        assert_eq!(split_card_cid("lkcx~a"), None);
    }

    #[test]
    fn a_body_frame_carries_its_senders_policy() {
        let body = make_body(Some("<p>Hi <img src=\"https://t.example/p.gif\"></p>"), None);
        let blocked = body_srcdoc(&body, 0, false, false, false);
        assert!(blocked.contains("img-src data: cid:;"), "blocked: {blocked}");
        assert!(body_srcdoc(&body, 0, true, false, false).contains("img-src * data: cid:"));
        assert!(body_srcdoc(&body, 0, true, true, false).contains("default-src *"));
        assert!(!blocked.contains("invert(1)"));
        assert!(body_srcdoc(&body, 0, false, false, true).contains("invert(1)"), "the dark override");
        let text = make_body(None, Some("a < b & c"));
        assert!(body_srcdoc(&text, 0, false, false, false).contains("a &lt; b &amp; c"));
    }

    #[test]
    fn a_hostile_body_cannot_break_out_of_its_frame() {
        let hostile = r#""></iframe><script>alert(1)</script><iframe srcdoc=""#;
        let member = Member {
            summary: summary(&inbox(), 1, 9, "a@x"),
            folder_label: None,
        };
        let html = card_html(0, &card(member, true, Some(hostile)), Utc::now());
        assert_eq!(html.matches("<iframe").count(), 1, "{html}");
        assert!(!html.contains("<script>"), "{html}");
        assert!(html.contains("data-srcdoc=\"&quot;&gt;&lt;/iframe&gt;&lt;script&gt;"), "{html}");
        assert!(html.contains("sandbox=\"allow-same-origin allow-popups\""));
        assert!(!html.contains("allow-scripts"));
    }

    #[test]
    fn card_markup_reflects_state() {
        let mut message = summary(&sent(), 7, 10, "mine@x");
        message.flags.insert(SystemFlagBit::Draft);
        message.importance = Importance::High;
        let member = Member {
            summary: message,
            folder_label: Some("Sent Items".into()),
        };
        let collapsed = card_html(2, &card(member.clone(), false, None), Utc::now());
        assert!(collapsed.starts_with("<article class=\"card\" id=\"card-2\" data-card=\"2\">"), "{collapsed}");
        assert!(collapsed.contains("aria-expanded=\"false\""));
        assert!(collapsed.contains(">Sent Items<") && collapsed.contains(">Draft<") && collapsed.contains(">High importance<"));
        assert!(collapsed.contains("data-action=\"reply\" disabled"), "a draft can't be replied to");
        assert!(collapsed.contains("Loading…"));
        let mut blocked = card(member, true, Some("<p>x</p>"));
        blocked.remote_blocked = true;
        blocked.attachments.push(AttachmentTile {
            part_number: "2".into(),
            name: "plan <v2>.pdf".into(),
            size_label: "3.0 KB".into(),
        });
        let expanded = card_html(0, &blocked, Utc::now());
        assert!(expanded.contains("class=\"card expanded\""));
        assert!(expanded.contains("data-action=\"show-blocked\""));
        assert!(expanded.contains("data-part=\"2\"") && expanded.contains("plan &lt;v2&gt;.pdf"));
        assert!(expanded.contains(">1 attachment<"));
    }

    #[test]
    fn the_document_csp_admits_the_most_trusting_card_only() {
        let member = Member {
            summary: summary(&inbox(), 1, 9, "a@x"),
            folder_label: None,
        };
        let body = make_body(Some("<p>x</p>"), None);
        let plain = vec![card(member.clone(), true, Some(&body_srcdoc(&body, 0, false, false, false)))];
        let doc = build_document(&plain, Utc::now(), false);
        assert!(doc.contains("script-src 'none'") && !doc.contains("default-src *"));
        assert!(doc.contains(">1 message<"));
        let trusting = vec![card(member, true, Some(&body_srcdoc(&body, 0, true, true, false)))];
        assert!(build_document(&trusting, Utc::now(), true).contains("default-src * data: cid: blob:"));
        assert!(build_document(&trusting, Utc::now(), true).contains("<body class=\"dark\">"));
    }

    #[test]
    fn script_messages_parse() {
        assert_eq!(parse_script_message(r#"{"action":"expand","card":2}"#), Some(ScriptMessage::Expand(2)));
        assert_eq!(parse_script_message(r#"{"action":"reply-all","card":0}"#), Some(ScriptMessage::ReplyAll(0)));
        assert_eq!(
            parse_script_message(r#"{"action":"menu","card":1,"x":10.5,"y":20}"#),
            Some(ScriptMessage::Menu { card: 1, x: 10.5, y: 20.0 })
        );
        assert_eq!(
            parse_script_message(r#"{"action":"attachment","card":1,"part":"1.2","x":1,"y":2}"#),
            Some(ScriptMessage::Attachment {
                card: 1,
                part: "1.2".into(),
                x: 1.0,
                y: 2.0
            })
        );
        assert_eq!(parse_script_message(r#"{"action":"explode","card":1}"#), None);
        assert_eq!(parse_script_message(r#"{"action":"reply"}"#), None);
        assert_eq!(parse_script_message("not json"), None);
    }

    #[test]
    fn script_calls_quote_their_payloads() {
        let member = Member {
            summary: summary(&inbox(), 1, 9, "a@x"),
            folder_label: None,
        };
        let call = js_set_body(&card(member, true, Some("<p>\"hi\"</p>\n")), 1);
        assert!(call.starts_with("lookout.setBody(1, \"<p>\\\"hi"), "{call}");
        assert!(call.contains("\\n"));
        assert_eq!(js_set_expanded(2, true), "lookout.setExpanded(2, true)");
        assert_eq!(js_scroll_to(4), "lookout.scrollTo(4)");
    }
}
