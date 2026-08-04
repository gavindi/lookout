use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::Duration;

use adw::prelude::*;
use gtk::glib;
use lookout_core::{EmailBody, EmailSummary};
use lookout_mail::session::AccountCommand;
use lookout_mail::{new_message_id, ComposedMessage};
use webkit::prelude::*;

/// How long the composer waits after the last check before autosaving the
/// draft again. The autosave runs on a fixed tick and compares the current
/// fields against the last saved snapshot, so this is the worst-case gap
/// between an edit and its draft landing server-side. A draft save is
/// several IMAP round trips (replace + append), so this errs slow.
const DRAFT_AUTOSAVE_INTERVAL: Duration = Duration::from_secs(5);

/// Everything the compose window can be pre-filled with, beyond a blank "New
/// Message" (`ComposePrefill::default()`). Grouped into one struct rather
/// than more loose parameters since Reply/Reply-All/Forward all need to set
/// several of these fields together.
#[derive(Default)]
pub struct ComposePrefill {
    pub to: Option<String>,
    pub cc: Option<String>,
    pub subject: Option<String>,
    pub body: Option<String>,
    /// RFC 5322 `In-Reply-To` - the bare Message-Id (no `<>`) of the message
    /// being replied to. `mail_builder`'s `MessageId` header writer adds the
    /// angle brackets itself, so this must NOT include them.
    pub in_reply_to: Option<String>,
    /// RFC 5322 `References` - bare Message-Ids (no `<>`), oldest first.
    pub references: Vec<String>,
}

/// Whether Reply excludes the original's other recipients (`Reply`) or
/// carries them all forward minus the replying account itself (`ReplyAll`).
#[derive(Clone, Copy)]
pub enum ReplyMode {
    Reply,
    ReplyAll,
}

/// Case-insensitive lookup against `EmailBody::headers` (raw RFC 5322
/// headers, verbatim - see that field's doc comment). IMAP's `ENVELOPE`
/// doesn't carry `References`, so this is the only place that header is
/// available at all; `Message-Id` is looked up the same way for consistency.
fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
}

/// Strips one layer of RFC 5322 `<...>` angle brackets from a Message-Id
/// token, if present. `mail_builder`'s `MessageId` header writer adds its
/// own brackets around each id it's given, so a raw header value (which
/// already has them) must be stripped first or the sent header would come
/// out double-bracketed.
fn strip_angle_brackets(id: &str) -> &str {
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
    }
}

/// Builds the Forward prefill: blank recipients (the user fills these in), a
/// `Fwd: `-prefixed subject, and a forwarded-message header block followed
/// by the original body verbatim. Unlike Reply, this starts a new,
/// unthreaded conversation - no `in_reply_to`/`references` are set.
pub fn build_forward_prefill(summary: &EmailSummary, body: &EmailBody) -> ComposePrefill {
    let subject = summary.subject.as_deref().map(|s| with_prefix_once(s, "Fwd: ")).unwrap_or_else(|| "Fwd: ".to_string());

    let from = summary.from.iter().map(|a| a.display_label()).collect::<Vec<_>>().join(", ");
    let to = summary.to.iter().map(|a| a.display_label()).collect::<Vec<_>>().join(", ");
    let original = body.text_body.as_deref().unwrap_or("");
    let forward_body = format!(
        "\n\n---------- Forwarded message ----------\nFrom: {}\nDate: {}\nSubject: {}\nTo: {}\n\n{}",
        from,
        summary.date.format("%a, %b %d, %Y at %I:%M %p"),
        summary.subject.as_deref().unwrap_or(""),
        to,
        original
    );

    ComposePrefill {
        to: None,
        cc: None,
        subject: Some(subject),
        body: Some(forward_body),
        in_reply_to: None,
        references: Vec::new(),
    }
}

/// HTML-escapes the characters that would otherwise break out of element
/// text. Applied to the plain-text prefill before it's embedded in the
/// editor document, so `&`, `<`, `>`, quotes and apostrophes stay literal.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
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
/// same content.
fn text_to_html(text: &str) -> String {
    let mut out = String::new();
    let mut para_lines: Vec<String> = Vec::new();
    let mut quote_lines: Vec<String> = Vec::new();

    for raw in text.lines() {
        if let Some(rest) = raw.strip_prefix('>') {
            flush_paragraph(&mut out, &mut para_lines);
            quote_lines.push(rest.strip_prefix(' ').unwrap_or(rest).to_string());
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
        } else if trimmed.is_empty() {
            flush_paragraph(&mut out, &mut para_lines);
        } else {
            para_lines.push(raw.to_string());
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

/// Reads the editor's content back out of the WebView with one
/// `evaluate_javascript` round trip, returning the normalized HTML and the
/// plain-text rendering. WebKit's `FontSize`/`ForeColor` editing commands
/// wrap their selection in `<font>` elements; the script rewrites those into
/// styled spans so the outgoing HTML is clean instead of full of obsolete
/// tags. Returns `None` if the evaluation fails (e.g. the document hasn't
/// finished loading), so callers can fall back to a known-good body rather
/// than dropping the message.
fn read_content(web_view: &webkit::WebView) -> Option<(String, String)> {
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
            return JSON.stringify({ html: document.body.innerHTML, text: document.body.innerText });
        }
    )()"#;

    let result = glib::MainContext::default().block_on(web_view.evaluate_javascript_future(READ_SCRIPT, None, None));
    let value = result.ok()?;
    let parsed: serde_json::Value = serde_json::from_str(&value.to_string()).ok()?;
    let html = parsed.get("html").and_then(|v| v.as_str()).unwrap_or("").to_string();
    let text = parsed.get("text").and_then(|v| v.as_str()).unwrap_or("").to_string();
    Some((html, text))
}

/// Adds a toolbar button that fires a plain WebKit editing command (bold,
/// italic, list, ...) on click.
fn toolbar_command_button(toolbar: &gtk::Box, icon_name: &str, tooltip: &str, command: &'static str, web_view: &Rc<webkit::WebView>) {
    let button = gtk::Button::builder().icon_name(icon_name).tooltip_text(tooltip).build();
    let web_view = web_view.clone();
    button.connect_clicked(move |_| web_view.execute_editing_command(command));
    toolbar.append(&button);
}

/// Splits a comma-separated recipient field (the To/Cc rows) into trimmed,
/// non-empty addresses.
fn parse_recipients(field: &str) -> Vec<String> {
    field.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()
}

/// Everything one draft-autosave needs to know to decide "did anything
/// change since the last save?" and to rebuild the message: the header rows
/// verbatim plus whichever body mode is live (`body` is the HTML in rich
/// mode and the plain text otherwise; `body_text` is always the plain-text
/// rendering).
#[derive(Clone, PartialEq)]
struct DraftSnapshot {
    to: String,
    cc: String,
    subject: String,
    rich: bool,
    body: String,
    body_text: String,
}

/// A draft with no recipients, no subject and no body content is not worth
/// storing - an untouched "New Message" composer must not litter the Drafts
/// folder with empty messages. Whitespace-only counts as empty; the rich
/// editor's blank document (`<p><br></p>`) renders to whitespace-only text,
/// so it's caught by the same check.
fn draft_is_trivial(snap: &DraftSnapshot) -> bool {
    snap.to.trim().is_empty() && snap.cc.trim().is_empty() && snap.subject.trim().is_empty() && snap.body_text.trim().is_empty()
}

/// Builds the rich-text editor page: a formatting toolbar above an editable
/// WebKit `WebView`. The whole page behaves as a contenteditable document,
/// so formatting actions map to WebKit editing commands (`Bold`,
/// `InsertUnorderedList`, `FontSize`, `ForeColor`, `CreateLink`, ...) and
/// the final content is read back with `evaluate_javascript` at send time
/// (see `read_content`). Navigation and remote subresources are vetoed
/// regardless of the reading pane's "Load images" setting - composing must
/// never fetch remote content. Returns the page plus the WebView, which the
/// caller keeps alive for send-time content reads.
fn build_rich_editor(initial_html: String) -> (gtk::Box, Rc<webkit::WebView>) {
    let settings = webkit::Settings::builder().enable_javascript(true).build();
    let web_view = Rc::new(webkit::WebView::builder().editable(true).settings(&settings).build());

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
    let link_button = gtk::Button::builder().icon_name("insert-link-symbolic").tooltip_text("Insert link").build();
    link_button.connect_clicked(move |button| link_dialog.present(Some(button)));
    toolbar.append(&link_button);

    let scroller = gtk::ScrolledWindow::builder().child(&*web_view).hexpand(true).vexpand(true).build();
    let page = gtk::Box::builder().orientation(gtk::Orientation::Vertical).spacing(6).build();
    page.append(&toolbar);
    page.append(&scroller);

    (page, web_view)
}

/// Builds a composer widget, meant to be embedded as a page in the reading
/// pane's `gtk::Stack` (see `window.rs`'s
/// `show_composer_in_reading_pane`) rather than presented as its own window.
/// `on_done` is called once Cancel or Send is clicked, so the caller can
/// tear the page down and restore whatever the reading pane showed before.
///
/// The body supports two modes, switched with the "Rich text" row: the
/// plain `gtk::TextView` fallback (also a send-time fallback for HTML
/// clients), and a contenteditable `WebKit.WebView` with a formatting
/// toolbar (see `build_rich_editor`). Only one mode is live at a time;
/// `rich_text_default` picks which one the composer opens on (Config → Mail).
/// The HTML mode sends `multipart/alternative` - both the rich HTML and a
/// plain-text rendering - so recipients without HTML support still get the
/// text.
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
/// exit.
pub fn build_compose_view(
    title: &str,
    from_email: String,
    cmd_tx: async_channel::Sender<AccountCommand>,
    prefill: ComposePrefill,
    on_done: Rc<dyn Fn()>,
    rich_text_default: bool,
) -> (gtk::Box, async_channel::Sender<String>) {
    let to_row = adw::EntryRow::builder().title("To").build();
    if let Some(to) = &prefill.to {
        to_row.set_text(to);
    }
    let cc_row = adw::EntryRow::builder().title("Cc").build();
    if let Some(cc) = &prefill.cc {
        cc_row.set_text(cc);
    }
    let subject_row = adw::EntryRow::builder().title("Subject").build();
    if let Some(subject) = &prefill.subject {
        subject_row.set_text(subject);
    }

    let rich_toggle = adw::SwitchRow::builder().title("Rich text").subtitle("Formatting (bold, lists, links, ...)").build();

    let fields_group = adw::PreferencesGroup::new();
    fields_group.add(&to_row);
    fields_group.add(&cc_row);
    fields_group.add(&subject_row);
    fields_group.add(&rich_toggle);

    let body_view = gtk::TextView::builder()
        .wrap_mode(gtk::WrapMode::WordChar)
        .left_margin(8)
        .right_margin(8)
        .top_margin(8)
        .bottom_margin(8)
        .build();
    if let Some(body) = &prefill.body {
        body_view.buffer().set_text(body);
    }
    let text_scroller = gtk::ScrolledWindow::builder().child(&body_view).hexpand(true).vexpand(true).build();

    let (rich_page, rich_web_view) = build_rich_editor(prefill.body.as_deref().map(text_to_html).unwrap_or_default());

    let body_stack = gtk::Stack::builder().hexpand(true).vexpand(true).build();
    body_stack.add_named(&text_scroller, Some("text"));
    body_stack.add_named(&rich_page, Some("rich"));
    body_stack.set_visible_child_name("text");

    {
        let web_view = rich_web_view.clone();
        let stack_for_toggle = body_stack.clone();
        rich_toggle.connect_active_notify(move |toggle| {
            if toggle.is_active() {
                stack_for_toggle.set_visible_child_name("rich");
                web_view.grab_focus();
            } else {
                stack_for_toggle.set_visible_child_name("text");
            }
        });
    }
    // Wire the Config → Mail preference into the composer's opening mode. The
    // notify handler above is connected first, so `set_active` also routes
    // the stack to the right page and focuses the editor.
    rich_toggle.set_active(rich_text_default);

    // --- Draft autosave state ---
    // One stable Message-ID per compose session: every autosave carries it,
    // and `replace` purges whatever the previous save stored under it, so
    // the Drafts folder only ever holds one copy of this draft.
    let draft_message_id = Rc::new(new_message_id());
    // Set the moment a SaveDraft is queued for this session - gates the
    // `replace` flag and the delete-before-send, so it must be optimistic
    // (a queued-but-failed save still means "ask the server to replace /
    // delete by Message-ID"; both are harmless no-ops if nothing is there).
    let draft_queued = Rc::new(Cell::new(false));
    // The last snapshot actually queued for saving; the autosave tick skips
    // work while the fields still match it.
    let last_saved: Rc<RefCell<Option<DraftSnapshot>>> = Rc::new(RefCell::new(None));
    // Set when the composer closes (Send or Cancel) so the autosave timer
    // can't fire against a torn-down page.
    let closed = Rc::new(Cell::new(false));

    let status_label = gtk::Label::builder().css_classes(["dim-label"]).build();

    let take_snapshot: Rc<dyn Fn() -> Option<DraftSnapshot>> = Rc::new({
        let to_row = to_row.clone();
        let cc_row = cc_row.clone();
        let subject_row = subject_row.clone();
        let rich_toggle = rich_toggle.clone();
        let body_view = body_view.clone();
        let rich_web_view = rich_web_view.clone();
        move || {
            let rich = rich_toggle.is_active();
            let (body, body_text) = if rich {
                // A failed read (document still loading) means "no snapshot
                // this tick" - the next one retries.
                read_content(&rich_web_view)?
            } else {
                let buffer = body_view.buffer();
                let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
                (text.clone(), text)
            };
            Some(DraftSnapshot {
                to: to_row.text().to_string(),
                cc: cc_row.text().to_string(),
                subject: subject_row.text().to_string(),
                rich,
                body,
                body_text,
            })
        }
    });

    let attempt_autosave: Rc<dyn Fn()> = Rc::new({
        let take_snapshot = take_snapshot.clone();
        let last_saved = last_saved.clone();
        let draft_queued = draft_queued.clone();
        let draft_message_id = draft_message_id.clone();
        let cmd_tx = cmd_tx.clone();
        let from_email = from_email.clone();
        let in_reply_to = prefill.in_reply_to.clone();
        let references = prefill.references.clone();
        let status_label = status_label.clone();
        move || {
            let Some(snap) = take_snapshot() else { return };
            if draft_is_trivial(&snap) {
                return;
            }
            if last_saved.borrow().as_ref() == Some(&snap) {
                return;
            }
            let replace = draft_queued.get();
            *last_saved.borrow_mut() = Some(snap.clone());
            draft_queued.set(true);
            let msg = ComposedMessage {
                from: from_email.clone(),
                to: parse_recipients(&snap.to),
                cc: parse_recipients(&snap.cc),
                bcc: Vec::new(),
                subject: snap.subject.clone(),
                text_body: snap.body_text.clone(),
                html_body: snap.rich.then(|| snap.body.clone()),
                in_reply_to: in_reply_to.clone(),
                references: references.clone(),
                message_id: Some((*draft_message_id).clone()),
            };
            let _ = cmd_tx.send_blocking(AccountCommand::SaveDraft { msg, replace });
            status_label.set_label("Saving draft…");
        }
    });

    // The autosave tick: fixed interval, change-detected by snapshot
    // comparison rather than edit signals (WebKit's contenteditable view has
    // no usable "content changed" signal, so polling is the only mode that
    // covers both body editors uniformly).
    {
        let attempt_autosave = attempt_autosave.clone();
        let closed = closed.clone();
        glib::spawn_future_local(async move {
            loop {
                glib::timeout_future(DRAFT_AUTOSAVE_INTERVAL).await;
                if closed.get() {
                    break;
                }
                attempt_autosave();
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

    let cancel_button = gtk::Button::builder().label("Cancel").build();
    let send_button = gtk::Button::builder().label("Send").css_classes(["suggested-action"]).build();
    let title_label = gtk::Label::builder().label(title).hexpand(true).build();

    let top_row = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(6)
        .margin_top(6)
        .margin_bottom(6)
        .margin_start(6)
        .margin_end(6)
        .build();
    top_row.append(&cancel_button);
    top_row.append(&title_label);
    top_row.append(&status_label);
    top_row.append(&send_button);

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
    content.append(&body_stack);

    {
        let on_done = on_done.clone();
        let attempt_autosave = attempt_autosave.clone();
        let closed = closed.clone();
        cancel_button.connect_clicked(move |_| {
            // One final save before teardown, so closing the composer never
            // loses work (the draft stays in Drafts; there's no "discard"
            // affordance yet). `closed` is set *after* the save so the
            // attempt isn't vetoed by it.
            attempt_autosave();
            closed.set(true);
            on_done();
        });
    }
    {
        let in_reply_to = prefill.in_reply_to;
        let references = prefill.references;
        let prefill_body_text = prefill.body.clone().unwrap_or_default();
        let draft_queued = draft_queued.clone();
        let draft_message_id = draft_message_id.clone();
        let closed = closed.clone();
        send_button.connect_clicked(move |_| {
            let to: Vec<String> = to_row.text().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if to.is_empty() {
                return;
            }
            let cc: Vec<String> = cc_row.text().split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            let (text_body, html_body) = if rich_toggle.is_active() {
                // Reading the editor's live HTML back out is a round trip
                // through WebKit; if that fails (e.g. the page hasn't
                // finished loading yet) fall back to the prefill body so a
                // Send click can never silently drop the message.
                match read_content(&rich_web_view) {
                    Some((html, text)) => {
                        let text = text.trim().to_string();
                        if text.is_empty() {
                            (String::new(), None)
                        } else {
                            let html = html.trim().to_string();
                            (text, Some(html))
                        }
                    }
                    None => (prefill_body_text.clone(), None),
                }
            } else {
                let buffer = body_view.buffer();
                let text = buffer.text(&buffer.start_iter(), &buffer.end_iter(), false).to_string();
                (text, None)
            };
            // If this message was draft-autosaved, delete the stored draft
            // first so the sent mail doesn't linger in Drafts. Queued ahead
            // of the send on the same command channel, so the session
            // processes them in order.
            if draft_queued.get() {
                let _ = cmd_tx.send_blocking(AccountCommand::DeleteDraft {
                    message_id: (*draft_message_id).clone(),
                });
            }
            let msg = ComposedMessage {
                from: from_email.clone(),
                to,
                cc,
                bcc: Vec::new(),
                subject: subject_row.text().to_string(),
                text_body,
                html_body,
                in_reply_to: in_reply_to.clone(),
                references: references.clone(),
                message_id: None,
            };
            let _ = cmd_tx.send_blocking(AccountCommand::SendMessage(msg));
            closed.set(true);
            on_done();
        });
    }

    (content, draft_saved_tx)
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
            preview: None,
        }
    }

    fn sample_body(headers: Vec<(String, String)>, text_body: Option<&str>) -> EmailBody {
        EmailBody {
            uid: Uid(1),
            text_body: text_body.map(str::to_string),
            html_body: None,
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
        assert_eq!(text_to_html("\n\n"), "");
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
    }

    fn snapshot(to: &str, subject: &str, body_text: &str) -> DraftSnapshot {
        DraftSnapshot {
            to: to.to_string(),
            cc: String::new(),
            subject: subject.to_string(),
            rich: false,
            body: body_text.to_string(),
            body_text: body_text.to_string(),
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
    }

    #[test]
    fn rich_blank_document_is_trivial() {
        // The rich editor's untouched document renders to whitespace-only
        // text even though its HTML is non-empty.
        let snap = DraftSnapshot {
            to: String::new(),
            cc: String::new(),
            subject: String::new(),
            rich: true,
            body: "<p><br></p>".to_string(),
            body_text: "\n".to_string(),
        };
        assert!(draft_is_trivial(&snap));
    }
}
